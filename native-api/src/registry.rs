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

// The ambiguous-vs-absent distinction, and the refusal a by-name lookup is
// allowed to return. See `class_identity` for why a native needs both.
use crate::class_identity::{ClassIdentityError, NameLookup};

/// NIO-SERVER-SOCKET (route 1): cached check of the `CRATONVM_REAL_NET_SOCKETS`
/// env var. When set, the native registry drops all synthetic
/// `java/net/Socket` / `java/net/ServerSocket` registrations so real JDK
/// bytecode drives the `sun/nio/ch/Net` path. Cached in a `OnceLock` because
/// `register()` is called thousands of times at startup.
fn real_net_sockets_enabled() -> bool {
    cratonvm_types::flags::flags().io.real_net_sockets
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
    cratonvm_types::flags::flags().natives.real_forkjoinpool
}

use rustc_hash::{FxHashMap, FxHashSet};

use cratonvm_types::compat::CompatibilityMode;
use cratonvm_types::error::{JdkOnlyViolation, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::ClassId;
use cratonvm_types::{ArrayElementType, ObjectKind};
use cratonvm_types::{ObjectRef, Value};

/// `CRATONVM_DBG_NATIVE_LOOKUPS=1` — **how many registry probes one invoke
/// costs**, which is the number the VM-wide per-call page asks for before
/// anyone restructures dispatch:
///
/// > A real fix is one lookup per invoke, handed down the chain — a
/// > restructuring of the dispatch entry points, not a change to the registry.
/// > Anyone starting there should first get the per-invoke lookup COUNT (there
/// > is no counter today; `CRATONVM_DBG_DISPATCH_TALLY` gives callees, not
/// > lookups per callee), because that number, not the profile share, is what a
/// > restructuring would divide.
///
/// A flat profile can only ever say what share `slot_for_exact` has; it cannot
/// say whether that share is one lookup per invoke (in which case a
/// restructuring divides it by one, i.e. buys nothing) or ten. This counts both
/// sides: every entry point into the registry, and every invoke that reaches
/// the two dispatchers those entry points hang off.
///
/// Off by default and gated on one relaxed atomic load, so a disabled run pays
/// a predictable branch per probe and nothing else. When enabled it costs a
/// contended `fetch_add` per probe — fine for a COUNT, useless for a timing.
pub mod lookup_census {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    /// `NativeMethodRegistry::find`.
    pub const FIND: usize = 0;
    /// `NativeMethodRegistry::find_with_kind`.
    pub const FIND_WITH_KIND: usize = 1;
    /// `NativeMethodRegistry::resolve_id` — the every-invoke entry point.
    pub const RESOLVE_ID: usize = 2;
    /// `NativeMethodRegistry::resolve_id_by_key` — the memoized-digest form.
    pub const RESOLVE_ID_BY_KEY: usize = 3;
    /// `resolve_id_with_descriptor_quirks` — the `#[cold]` rewrite arm.
    pub const QUIRKS: usize = 4;
    /// One bytecode-level invoke reaching `try_stackless_invoke`.
    ///
    /// **This is NOT a general per-invoke denominator, and `lookups_per_invoke`
    /// must not be read as "registry probes per Java call".** Measured
    /// 2026-08-18 on `probes/LambdaCompositionProbe.java` at four workload
    /// sizes: `find` scaled perfectly linearly (151 254 / 231 254 / 391 254 /
    /// 711 254) while this counter stayed pinned at **966 in all four runs**.
    /// The lookups that workload generates do not come through this entry
    /// point at all, so the printed ratio grew 162 -> 742 purely because the
    /// numerator moved and the denominator could not.
    ///
    /// The reliable reading is the MARGINAL rate: run two sizes and divide the
    /// difference in `find` by the difference in work. That gave exactly 4.0
    /// lookups per composition stage, with a fixed ~71 k boot cost — a fact
    /// the ratio line could not have produced at any single size.
    pub const INVOKE_STACKLESS: usize = 5;
    /// One call reaching `invoke_or_native`, the general resolver.
    pub const INVOKE_GENERAL: usize = 6;

    const N: usize = 7;
    const NAMES: [&str; N] = [
        "find",
        "find_with_kind",
        "resolve_id",
        "resolve_id_by_key",
        "quirks",
        "invokes(stackless)",
        "invokes(general)",
    ];

    static COUNTS: [AtomicU64; N] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    static INIT: OnceLock<bool> = OnceLock::new();

    #[inline]
    fn enabled() -> bool {
        // The already-initialised load first: after the first probe this is a
        // relaxed pointer read and a compare, which is what a per-invoke path
        // can afford. `get_or_init` runs once.
        if let Some(v) = INIT.get() {
            return *v;
        }
        *INIT.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NATIVE_LOOKUPS").is_some()
        })
    }

    /// Count one probe of `kind`. Inlined so the disabled case is a load and a
    /// not-taken branch.
    #[inline]
    pub fn probe(kind: usize) {
        if !enabled() {
            return;
        }
        record(kind);
    }

    #[cold]
    fn record(kind: usize) {
        let n = COUNTS[kind].fetch_add(1, Ordering::Relaxed) + 1;
        // Report on the INVOKE counters only: they are the denominators, so a
        // line is emitted at a round number of invokes rather than at a round
        // number of lookups, and every line is directly comparable.
        if kind == INVOKE_STACKLESS && n % 50_000_000 == 0 {
            report("periodic");
        }
    }

    /// Zero every counter.
    pub fn reset() {
        for c in COUNTS.iter() {
            c.store(0, Ordering::Relaxed);
        }
    }

    /// Tally of MISSED lookup triples — the question the ratio cannot answer:
    /// not "how many", but "which".
    ///
    /// A miss is `find` returning `None` after both the exact probe and the
    /// descriptor-quirk rewrite failed. Measured on `LambdaCompositionProbe`
    /// those are ~100% of all `find` calls and exactly 4.0 per composition
    /// stage, so the top rows here name the four — and a triple like
    /// `java/util/concurrent/CompletableFuture.thenApply` identifies its caller
    /// far more directly than a stack would. Which matters, because `perf`'s
    /// dwarf unwinding through these frames yields bogus return addresses and
    /// gives no callers at all.
    ///
    /// Behind the same `CRATONVM_DBG=native-lookups` gate, and allocating only
    /// when it is on.
    static MISSES: OnceLock<parking_lot::Mutex<std::collections::HashMap<String, u64>>> =
        OnceLock::new();

    /// Record one missed triple. Gated; a disabled run does not allocate.
    #[inline]
    pub fn note_miss(class_name: &str, method_name: &str, descriptor: &str) {
        if !enabled() {
            return;
        }
        record_miss(class_name, method_name, descriptor);
    }

    #[cold]
    fn record_miss(class_name: &str, method_name: &str, descriptor: &str) {
        let map = MISSES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
        let key = format!("{class_name}.{method_name}{descriptor}");
        *map.lock().entry(key).or_insert(0) += 1;
    }

    /// The `n` most-missed triples, hottest first.
    fn top_misses(n: usize) -> Vec<(String, u64)> {
        let Some(map) = MISSES.get() else {
            return Vec::new();
        };
        let mut v: Vec<(String, u64)> = map.lock().iter().map(|(k, c)| (k.clone(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Print the census and the ratio it exists to produce. Safe to call when
    /// disabled — it prints nothing.
    ///
    /// **Read `lookups_per_invoke` with the caveat on [`INVOKE_STACKLESS`].**
    /// It is lookups over *stackless-entry invokes*, not over Java calls, and
    /// on a workload whose lookups arrive by another path the denominator is
    /// constant while the numerator scales — which makes the ratio grow with
    /// the workload and mean nothing. Take two sizes and use the marginal
    /// rate.
    pub fn report(tag: &str) {
        if !enabled() {
            return;
        }
        let v: Vec<u64> = COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        let lookups: u64 = v[FIND] + v[FIND_WITH_KIND] + v[RESOLVE_ID] + v[RESOLVE_ID_BY_KEY];
        let invokes = v[INVOKE_STACKLESS];
        let per_invoke = if invokes == 0 {
            0.0
        } else {
            lookups as f64 / invokes as f64
        };
        let mut parts = String::new();
        for (i, name) in NAMES.iter().enumerate() {
            parts.push_str(&format!(" {name}={}", v[i]));
        }
        eprintln!(
            "[native-lookups {tag}] lookups={lookups} invokes={invokes} \
             lookups_per_invoke={per_invoke:.2}{parts}"
        );
        for (triple, count) in top_misses(12) {
            eprintln!("[native-lookups {tag}] miss {count:>10}  {triple}");
        }
    }
}

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
    /// Cumulative monitor-enter blocking, measured only while contention
    /// monitoring is enabled. Durations are milliseconds per the JMM API.
    pub blocked_time_ms: i64,
    pub blocked_count: i64,
    /// Cumulative Object.wait parking, also in JMM milliseconds/counts.
    pub waited_time_ms: i64,
    pub waited_count: i64,
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
    /// The JVMS §4.7.9 `Signature` attribute of this method, i.e. its
    /// generic signature, or `None` when the method is not generic.
    ///
    /// Carried here for the same reason as `exceptions`: whoever produced
    /// this metadata already walked the declaring class's method table, and
    /// `create_method_object` would otherwise search that table AGAIN, by
    /// name+descriptor, once per mirror. In a `Class.getDeclaredMethods()`
    /// call that loop already runs once per method, so the search made the
    /// cost per method grow with the method count — 766 us of a 2440 us call
    /// on a 1000-method class went to the exceptions lookup and 659 us to
    /// this one. A fabricated method (a synthetic stub, a lambda-proxy SAM,
    /// a shim) is not generic and correctly reports `None`.
    pub signature: Option<String>,
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
    /// Exact superclass identity already resolved through the defining
    /// lookup/loader.
    pub superclass_id_override: Option<ClassId>,
    /// Exact interface identities, in class-file declaration order.
    pub interface_id_overrides: Option<Vec<ClassId>>,
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

/// Why (or whether) a `LambdaMetafactory`-spun proxy is `java.io.Serializable`.
///
/// The JDK decides this in `AbstractValidatingLambdaMetafactory`: a lambda is
/// serializable when its call site passed `FLAG_SERIALIZABLE` **or** its
/// functional interface already extends `Serializable`, and the spun class gets
/// `Serializable` *added* to its interface list only in the first case. Each arm
/// drives a different reflective surface, so the distinction is modelled here
/// rather than flattened to a bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaSerializability {
    /// Not serializable: no `writeReplace()`, no `Serializable` in
    /// `getInterfaces()`, and `(Serializable) lambda` throws
    /// ClassCastException. This is the common case -- every plain
    /// `metafactory` lambda over a non-`Serializable` interface, e.g.
    /// `Supplier<String> s = () -> "x"`.
    NotSerializable,
    /// Serializable because the functional interface itself extends
    /// `Serializable`. Gets a `writeReplace()`, but `getInterfaces()` still
    /// reports only the functional interface -- `Serializable` is already
    /// reachable through it, so the metafactory adds no marker.
    ByInheritance,
    /// Serializable because the call site passed `FLAG_SERIALIZABLE` -- an
    /// `(Iface & Serializable)` intersection cast, which is how every JDK
    /// `Comparator.comparing*` factory is written. Gets a `writeReplace()`
    /// AND `java.io.Serializable` appended to `getInterfaces()` /
    /// `getGenericInterfaces()`.
    ByFlag,
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

/// Why a GPU submission failed, recorded at the point of failure.
///
/// `craton.gpu` documents three `GpuException` subclasses for targeted
/// `catch` clauses, and until this enum existed the only way to pick one
/// was to match substrings against the driver's error string on the Java
/// side — a heuristic that misfires on any wording nobody anticipated.
/// Stamping the category where the failure actually happens makes the
/// classification exact.
///
/// The discriminants are wire values: they are returned verbatim by
/// `Native.futureErrorKind` and MUST stay in step with the
/// `NativeBridge.ERROR_KIND_*` constants on the Java side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum GpuErrorKind {
    /// Not classified, or a failure none of the categories below fit.
    /// The Java side falls back to message matching on this value.
    #[default]
    Unknown = 0,
    /// The kernel could not be produced or loaded: the class would not
    /// resolve, the method was rejected by the analyzer, PTX would not
    /// compile, or the module loaded without the expected entry point.
    Compile = 1,
    /// A device-side allocation failed.
    OutOfMemory = 2,
    /// The kernel was launched, or a device operation was attempted, and
    /// the driver reported a failure.
    Launch = 3,
}

impl GpuErrorKind {
    /// The wire value handed to Java by `Native.futureErrorKind`.
    #[must_use]
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Recover a kind from its wire value; anything unrecognised
    /// degrades to [`GpuErrorKind::Unknown`] rather than panicking, so a
    /// newer Java side talking to an older VM still gets a usable answer.
    #[must_use]
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::Compile,
            2 => Self::OutOfMemory,
            3 => Self::Launch,
            _ => Self::Unknown,
        }
    }
}

/// [`NativeContext::gpu_future_await`] result: this VM has no timed wait.
///
/// Zero, deliberately — it is the value a context that does not override
/// the method returns, and it tells the caller to fall back to polling.
/// Mirrors `NativeBridge.AWAIT_UNSUPPORTED` on the Java side.
pub const GPU_AWAIT_UNSUPPORTED: i32 = 0;
/// [`NativeContext::gpu_future_await`] result: the submission completed.
pub const GPU_AWAIT_COMPLETED: i32 = 1;
/// [`NativeContext::gpu_future_await`] result: the timeout elapsed first.
pub const GPU_AWAIT_TIMED_OUT: i32 = 2;

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
    ///
    /// Allocates. Prefer [`Self::class_name_arc_of_id`] unless an owned
    /// `String` is genuinely what the caller needs.
    fn class_name_of_id(&self, class_id: ClassId) -> Option<String>;

    /// The `SourceFile` attribute of `class_id`, or `None` when the class
    /// declares none (or is not loaded).
    ///
    /// Added so a `StackWalker` frame carrier can store a `ClassId` and build
    /// its file name only if someone asks: `StackFrame.getFileName()` is read
    /// for roughly one frame of a `filter(..).findFirst()` walk, and building
    /// it eagerly cost a Java `String` allocation for every frame on the stack.
    /// Default `None` so the many test `NativeContext` mocks are unaffected —
    /// a carrier that gets `None` here answers `null`, exactly as one built
    /// from a frame with no source file always did.
    fn class_source_file(&self, class_id: ClassId) -> Option<String> {
        let _ = class_id;
        None
    }

    /// The class name for a `ClassId`, **without allocating**.
    ///
    /// The VM stores a class's name as an `Arc<str>` and
    /// [`Self::class_name_of_id`] copies it into a fresh `String` on every
    /// call. That copy is pure waste at the overwhelmingly common shape —
    /// `ctx.class_name_arc_of_id(cid).as_deref() == Some("java/util/HashMap")`
    /// — where the name is compared and dropped. This hands back a clone of the
    /// `Arc` instead: one refcount increment, no allocation, no copy. And
    /// `Option<Arc<str>>::as_deref()` yields exactly the `Option<&str>` the old
    /// shape did, so converting a call site is a drop-in the compiler checks.
    ///
    /// `native-collections` measured what the `String` costs: its receiver
    /// classification paid 5-6 of these per `HashMap.put`, which is part of the
    /// 21.2x row against JDK 25 C2 that its `RECEIVER_FACTS` / `RECEIVER_NAMES`
    /// memos exist to remove. Those memos also remove the `class_manager` read
    /// lock, which this does NOT — a caller on a path hot enough to care about
    /// the lock still wants a memo, and this is not a substitute for one.
    ///
    /// Default-implemented in terms of `class_name_of_id` so every existing
    /// `NativeContext` (six test mocks among them) keeps compiling unchanged;
    /// the VM overrides it with an `Arc::clone`.
    fn class_name_arc_of_id(&self, class_id: ClassId) -> Option<Arc<str>> {
        self.class_name_of_id(class_id).map(Arc::from)
    }

    /// Get the class id of a heap object.
    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId;

    /// True when the named class is loaded as a synthetic stub (no real
    /// `.class` bytes). Used to branch native helpers that must mirror JDK
    /// behaviour without registering natives that would override real JDK
    /// bytecode once the stub upgrades.
    /// Would a name-based load of `name` be answered with a *fabricated
    /// synthetic stub* (a class with no `Code` on any method, minted only so
    /// enterprise bytecode can link) rather than a real class?
    ///
    /// Non-destructive: unlike [`Self::is_class_synthetic_stub`] it does not
    /// load anything, so it can be used to decide whether to consult a real
    /// `ClassLoader` *before* the stub is minted and registered globally.
    /// Default `false` for contexts with no class manager.
    fn would_fabricate_synthetic_stub(&self, name: &str) -> bool {
        let _ = name;
        false
    }

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
        // Whether the call site passed `LambdaMetafactory.FLAG_SERIALIZABLE`.
        // The reflective `metafactory` entry point has no flags word and passes
        // `false`; such a lambda can still be serializable by inheritance.
        serializable: bool,
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
            serializable,
        );
        0
    }

    /// Record the `LambdaMetafactory.altMetafactory` MARKER INTERFACES of a
    /// proxy just returned by `register_lambda_proxy`.
    ///
    /// `altMetafactory`'s `FLAG_MARKERS` (0x2) block names interfaces the spun
    /// proxy implements IN ADDITION to its functional interface, so
    /// `marker.isInstance(lambda)` and a `checkcast` to the marker must both
    /// succeed. They are not part of the SAM metadata `register_lambda_proxy`
    /// carries, so they are handed over separately, right after it.
    ///
    /// `proxy_class_id` is the raw `u32` that `register_lambda_proxy` returned
    /// (`0` means it declined — nothing to record). `marker_interfaces` are
    /// internal names (`java/lang/Cloneable`). No-op by default: a host with no
    /// lambda-proxy table (test mocks) has nowhere to put them.
    fn register_lambda_proxy_markers(&mut self, proxy_class_id: u32, marker_interfaces: &[String]) {
        let _ = (proxy_class_id, marker_interfaces);
    }

    /// Check if child_class is a subclass of parent_class.
    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool;

    /// Is an instance of `class_id` assignable to `target_class_name`, asked
    /// the LOADER-BLIND way the bytecode asks it?
    ///
    /// This is the third door onto one rule, and it exists for the reason
    /// [`synthetic_implements_declared`] and [`aastore_element_assignable`]
    /// exist: the walk was written once, for `checkcast`/`aastore`, and every
    /// reflective caller that asks a SECOND, narrower question is how two doors
    /// come to disagree about one object.
    ///
    /// [`is_subclass`] is not that question. It compares `ClassId`s, so under a
    /// forked loader it refuses a value whose class chain names the target
    /// under a different id — and while it does walk the interface DAG, it
    /// walks the one recorded on the value's own class, which for a
    /// `@CompileWithForkedClassLoader` copy can name the OTHER loader's
    /// interface. `ClassManager::is_assignable_to_name` compares NAMES over
    /// supers and interfaces transitively, is cycle-safe and depth-capped, and
    /// is what `typecheck::aastore_element_assignable` already uses for exactly
    /// this case.
    ///
    /// # `None` is the honest answer, and callers must treat it as "allow"
    ///
    /// `None` means this context cannot answer — the default for a host with no
    /// VM hierarchy behind it (mocks, the fabricated-class harnesses). A caller
    /// that turns a `false` into a refusal must leave `None` alone, or a test
    /// mock starts throwing `ClassCastException` at correct code.
    ///
    /// **This does NOT carry the proxy hatches.** A `java.lang.reflect.Proxy`
    /// instance and a lambda proxy acquire their interfaces at RUNTIME,
    /// invisibly to any static walk, and `aastore_element_assignable` has a
    /// separate block for each. A caller that refuses on `Some(false)` must
    /// apply those itself.
    ///
    /// [`synthetic_implements_declared`]: Self::synthetic_implements_declared
    /// [`aastore_element_assignable`]: Self::aastore_element_assignable
    /// [`is_subclass`]: Self::is_subclass
    fn class_assignable_to_name(&self, class_id: ClassId, target_class_name: &str) -> Option<bool> {
        let _ = (class_id, target_class_name);
        None
    }

    /// Does the VM's `checkcast`/`instanceof` admit an instance of `class_id`
    /// as a `target_class_name` on a relationship that is **declared** rather
    /// than present in the loaded class hierarchy?
    ///
    /// This is the reflection door onto the interpreter's
    /// `typecheck::synthetic_implements` — the table that says a
    /// `cratonvm/internal/SystemLogger` is a `System.Logger` and a
    /// `cratonvm/internal/foreign/MemorySegmentImpl` is both a `MemorySegment`
    /// and an `AbstractMemorySegmentImpl`. Those classes are concrete stand-ins
    /// the VM mints itself, so the relationship exists nowhere a hierarchy walk
    /// can find it.
    ///
    /// It exists for the same reason [`aastore_element_assignable`] does: the
    /// rule was already written once, for the bytecode, and reflection asking a
    /// SECOND, narrower question is how the two doors come to disagree about
    /// one object. Measured, before this: `probes/FfmVectorSegmentProbe.java`'s
    /// IDENTITY section reported `abstractBase=false` for a segment that a
    /// `checkcast jdk/internal/foreign/AbstractMemorySegmentImpl` three frames
    /// away had just admitted.
    ///
    /// `false` by default: a host with no interpreter behind it (the test
    /// mocks) has no table to consult, and failing closed leaves the caller's
    /// own hierarchy checks as the only answer — which is what it had before
    /// this method existed.
    ///
    /// [`aastore_element_assignable`]: Self::aastore_element_assignable
    fn synthetic_implements_declared(&self, class_id: ClassId, target_class_name: &str) -> bool {
        let _ = (class_id, target_class_name);
        false
    }

    /// Get the superclass ClassId. Returns None for java/lang/Object.
    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId>;

    /// JVMS §aastore covariance: may the non-null `value` be stored into the
    /// reference array `array`? `None` when this context cannot answer.
    ///
    /// This exists so `java.lang.reflect.Array.set` can enforce **the same rule
    /// as the `aastore` bytecode**, which is what HotSpot does — they are one
    /// check there, and were two here. The VM's implementation
    /// (`vm::runtime::interpreter::typecheck::aastore_element_assignable`) is
    /// already shared by the interpreter opcode and the JIT's `jit_aastore`
    /// helper; a reflective store is simply its third caller.
    ///
    /// The difference is not academic. That predicate is deliberately
    /// *additive*: it must never produce a FALSE `ArrayStoreException`, so it
    /// fails open for an interface component, for a `$Proxy`/`AnnotationProxy`
    /// value, for a synthetic class id, and for a same-named component that
    /// resolved to a different `ClassId` under another loader. Every one of
    /// those hedges was paid for by a real regression. A `ClassId`-identity
    /// check reproduces none of them.
    ///
    /// `None` (the default) means "no VM hierarchy here" — mocks and the
    /// fabricated-class harnesses — and leaves the caller on its own exact
    /// check rather than silently widening it.
    fn aastore_element_assignable(&self, array: ObjectRef, value: ObjectRef) -> Option<bool> {
        let _ = (array, value);
        None
    }

    /// Get the ClassId for a loaded class by name. Returns None if not loaded.
    ///
    /// # `None` is two answers
    ///
    /// It means *either* "no loader has a class under this name" *or* "several
    /// distinct classes do, and with no initiating loader to disambiguate
    /// there is no correct one to return". Those demand opposite actions from
    /// a caller that reacts to a miss by loading or fabricating, so a caller
    /// that does anything other than give up should ask
    /// [`Self::classify_class_name`] instead.
    fn class_id_by_name(&self, name: &str) -> Option<ClassId>;

    /// The loader-aware form of [`Self::class_id_by_name`]: tells *why* there
    /// is no unique answer.
    ///
    /// Consult this — rather than the `Option` — from any native that would
    /// **act** on a miss (load the class, mint a stand-in, fall back to a
    /// same-named class of its own). Acting on
    /// [`NameLookup::Ambiguous`] adds one more class to a name that already
    /// has too many, and the new one usually outranks the others.
    ///
    /// # Default implementation
    ///
    /// Delegates to [`Self::class_id_by_name`], so a context that cannot tell
    /// the two apart keeps answering exactly what it answers today (`Absent`
    /// for both). That is the honest default for mocks and non-VM contexts:
    /// they have no name index and no ambiguity to report. The VM's own
    /// context overrides it against the class manager's `classify_loaded_name`,
    /// which is O(1) against the definition-count index.
    fn classify_class_name(&self, name: &str) -> NameLookup {
        match self.class_id_by_name(name) {
            Some(id) => NameLookup::Unique(id),
            None => NameLookup::Absent,
        }
    }

    /// Whether `class_id`'s static initializer has already run to completion.
    ///
    /// This is the query behind `Unsafe.shouldBeInitialized` /
    /// `shouldBeInitialized0`, which must answer "does this class still need
    /// initializing?". Before this existed, those natives returned a flat
    /// `false` ("everything is already initialized") because the VM's real
    /// `is_class_initialized_*` helpers live in the `vm` crate and were not
    /// reachable from `native-builtins` — a constant that is right for an
    /// initialized class and silently wrong for every other one.
    ///
    /// Deliberately distinct from `ensure_class_initialized`, which ACTS:
    /// calling that to answer the question would run the very `<clinit>` the
    /// caller is asking about, which is precisely what `Unsafe`'s callers are
    /// trying to avoid.
    ///
    /// The default returns `true` so implementations that do not track
    /// initialization state keep the previous observable behaviour instead of
    /// suddenly reporting every class as uninitialized.
    fn is_class_initialized(&self, _class_id: ClassId) -> bool {
        true
    }

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
    /// bug-h2-suite-residual-fail-triage-FIXED.md's
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

    /// For a synthetic lambda-proxy `ClassId`, return the resolved `ClassId`
    /// of its functional (SAM) interface when the invokedynamic bootstrap
    /// recorded one.  This preserves defining-loader identity for reflection
    /// paths that must not re-resolve a potentially ambiguous interface name.
    fn lambda_functional_interface_id(&self, _class_id: ClassId) -> Option<ClassId> {
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

    /// For a synthetic lambda-proxy `ClassId`, return the full lambda
    /// call-site metadata required to serialize and later reconstruct the
    /// lambda (see [`LambdaSerialMetadata`]). Returns `None` for any
    /// non-lambda class. Used by the object-serialization natives to emit a
    /// `SerializedLambda`-equivalent record instead of attempting to serialize
    /// the (un-loadable) synthetic `$$Lambda` proxy class by name.
    fn lambda_proxy_serial_metadata(&self, _class_id: ClassId) -> Option<LambdaSerialMetadata> {
        None
    }

    /// How this lambda proxy is (or is not) `java.io.Serializable`, per the
    /// JDK's own `LambdaMetafactory` rule. `NotSerializable` for any class that
    /// is not a lambda proxy.
    ///
    /// Real HotSpot spins a private `writeReplace()` and adds `Serializable` to
    /// the proxy's interfaces only for serializable lambdas, so every reflective
    /// `Class` surface must agree with this rather than assume all lambdas are
    /// serializable (which is what CratonVM did before 2026-08-01).
    fn lambda_proxy_serializability(&self, _class_id: ClassId) -> LambdaSerializability {
        LambdaSerializability::NotSerializable
    }

    /// Get the ClassLoaderId for a loaded class.
    /// Returns 0 = Bootstrap, 1 = Extension, 2 = Application, 3+ = UserDefined(id).
    fn loader_id_of_class(&self, class_id: ClassId) -> i32;

    /// Check if a class is a record (has Record attribute, JEP 395).
    fn is_record_class(&self, class_id: ClassId) -> bool;

    /// Get the record components (name, descriptor) for a record class.
    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)>;

    /// `hashCode`/`equals` fast-path metadata for `class_id`, resolved in a
    /// single class-manager lock acquisition:
    ///
    /// * `.0` — a bitmask describing how instances can be hashed/compared
    ///   without a Java dispatch:
    ///   * bit 0 — declares the **javac-generated** (JEP 395) `hashCode()I`
    ///   * bit 1 — declares the generated `equals(Ljava/lang/Object;)Z`
    ///   * bit 2 — declares the generated `toString()Ljava/lang/String;`
    ///   * bit 3 — instances have identity `hashCode`/`equals` semantics
    ///   * bit 4 — the class is `java.lang.String`
    ///   * bit 5 — the class is exactly `java.util.ArrayList`
    ///
    ///   The record bits are `0` for a non-record and for any of the three a
    ///   record hand-writes — an explicit override must keep its own
    ///   semantics. Bit 3 is set only for enum classes, where JLS §8.9 makes
    ///   `Enum.hashCode`/`Enum.equals` final and identity-based. Bits 3-5 are
    ///   mutually exclusive with the record bits and with each other.
    /// * `.1` — the field slot of the first record component, i.e. the class's
    ///   `first_field_index`. Always `0` for a real record (neither
    ///   `java.lang.Record` nor `java.lang.Object` declares an instance
    ///   field), but read rather than assumed.
    /// * `.2` — the number of record components, `0` for a non-record.
    ///
    /// Consulted once per component of every nested record by the direct
    /// record `hashCode`/`equals` implementations. Defaults to
    /// `(0, 0, 0)` — "no fast path" — for the test doubles.
    fn object_method_fast_path(&self, class_id: ClassId) -> (u8, usize, usize) {
        let _ = class_id;
        (0, 0, 0)
    }

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

    /// Do `bytes` fingerprint-match the class file `class_id`'s current
    /// definition came from?
    ///
    /// `None` means the VM recorded no fingerprint and cannot adjudicate.
    /// Used by `Instrumentation.retransformClasses` when `class_bytes` misses
    /// (that cache is size-capped and FIFO-evicted) and the retransformation
    /// base has to be re-read from the classpath: the classpath is searched
    /// bootstrap → extension → application, *not* through the class's defining
    /// loader, so what it returns can be a different build of the same name.
    /// Matching `this_class` does not catch that; this does. Default impl
    /// returns `None` so test mocks compile.
    fn class_bytes_match_base(&self, _class_id: ClassId, _bytes: &[u8]) -> Option<bool> {
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

    /// The incremental form of [`Self::find_all_resource_urls`]: the next
    /// matching URL at or after the cursor `(segment, index)`, plus the cursor
    /// to resume at.
    ///
    /// Enumerating from `(0, 0)` to exhaustion yields exactly what
    /// `find_all_resource_urls` yields, in the same order. It exists so that a
    /// consumer which stops early stops the classpath SCAN early — the JDK's
    /// `getResources` enumeration is lazy per element, and
    /// `resources(name).anyMatch(..)` is the common shape that depends on it.
    ///
    /// Only valid for names [`Self::resource_name_supports_incremental_scan`]
    /// accepts. Default returns `None` so test mocks compile.
    fn next_resource_url(
        &self,
        name: &str,
        segment: u32,
        index: u32,
    ) -> Option<(String, u32, u32)> {
        let _ = (name, segment, index);
        None
    }

    /// [`Self::find_all_resource_urls`] restricted to ONE class-path segment:
    /// 0 bootstrap, 1 extension, 2 application — the numbering
    /// [`Self::next_resource_url`] already uses.
    ///
    /// Exists for `ClassLoader.getDefinedPackage`, which does NOT delegate: the
    /// application loader must answer `null` for `java.lang`, which lives on
    /// segment 0. The default falls back to the unsegmented probe so that an
    /// implementation which has not overridden it behaves exactly as before
    /// rather than silently reporting "nothing is visible".
    fn find_resource_urls_in_segment(&self, name: &str, segment: u8) -> Vec<String> {
        let _ = segment;
        self.find_all_resource_urls(name)
    }

    /// `true` when [`Self::next_resource_url`] can serve `name` — i.e. the
    /// name matches at most one entry per classpath entry, so "the first URL
    /// this entry serves" cannot be dropping others. Default `false`.
    fn resource_name_supports_incremental_scan(&self, name: &str) -> bool {
        let _ = name;
        false
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

    /// Retract paths previously passed to [`Self::register_dynamic_classpath`].
    ///
    /// The other half of `URLClassLoader.close()`: a closed loader must stop
    /// serving classes and resources it has not already loaded. Classes already
    /// defined stay defined, matching HotSpot — `close()` shuts the loader's
    /// `URLClassPath`, it does not unload anything.
    ///
    /// Returns the number of classpath entries removed. A path handed to more
    /// than one live loader is only retracted when the last of them releases it,
    /// so `0` is a normal answer and not an error.
    ///
    /// The default is `0` ("this context cannot retract"), which is the
    /// behaviour every caller had before the VM implementation existed.
    fn unregister_dynamic_classpath(&mut self, _paths: &[String]) -> usize {
        0
    }

    /// Reset per-thread JMM blocked/waited counters when contention monitoring
    /// is enabled. Contexts without a live VM need no bookkeeping.
    fn reset_thread_jmx_contention_stats(&mut self) {}

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
    /// `loader_id` is the flat wire encoding of a
    /// [`cratonvm_types::ClassLoaderId`] — the same one
    /// [`NativeContext::loader_id_of_class`] produces, so a loader id fetched
    /// from an existing class can be handed straight back here to define a new
    /// class in that *same* loader (which is exactly what the CGLIB
    /// `@Configuration` enhancer does). Implementations MUST decode it with
    /// `ClassLoaderId::from_native_id_or_default`: `1` is the extension
    /// loader, `2` the application loader, anything `>= 3` a user-defined
    /// namespace, and `0` means "unspecified — use the application loader"
    /// (several callers pass a literal `0` for that). Decoding `2` as
    /// `UserDefined(2)` puts the new class in a different runtime package from
    /// its own superclass and silently breaks package-private override
    /// detection — see
    /// `configproxy-cglib-loaderid-fixed-20260727.md`.
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
    /// application loader (or, if `loader_id != 0`, the loader that
    /// `loader_id` names under the same wire encoding
    /// [`NativeContext::define_class_full`] documents).
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

    /// True if the VM's `ModuleRegistry` holds a descriptor for `module_name`.
    ///
    /// The default is `false` ("I know nothing"), so a `NativeContext` that does
    /// not model modules can never be read as asserting a module's ABSENCE —
    /// callers must probe a known-present name (`java.base`) before treating a
    /// `false` here as evidence.
    fn module_is_registered(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// Every module name the VM's module registry knows, in no defined order.
    ///
    /// The registry is small by construction, so callers may enumerate it
    /// eagerly. An empty vector means "this context does not model modules",
    /// exactly as [`module_is_registered`] returning `false` does, and must not
    /// be read as "no modules exist".
    ///
    /// It is NOT just "`java.base` plus whatever `--module-path` supplied", and
    /// it is NOT populated by only two sites. A THIRD site scans the
    /// APPLICATION CLASS PATH for `module-info.class` and registers what it
    /// finds — so a modular jar on `-cp`, which a real JVM treats as an
    /// unnamed-module citizen whose `module-info` is ignored, appears here as a
    /// module. That false sentence is what made a real defect look impossible
    /// from this trait's side: registering such a jar's services twice made
    /// `ServiceLoader` throw "multiple engines with the same ID". Use
    /// [`Self::module_is_class_path_only`] below to tell the two apart.
    /// docs/known-issues/jdk-only/E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md
    fn module_names(&self) -> Vec<String> {
        vec![]
    }

    /// True when `module_name` reached the registry ONLY by scanning the
    /// application class path — i.e. a modular jar on `-cp`, which a real JVM
    /// treats as an unnamed-module citizen whose `module-info` is ignored.
    ///
    /// Callers that model the JDK's module *system* must skip these; callers
    /// that only want labelling may not care. Defaults to `false` so a context
    /// that does not model modules keeps its existing behaviour.
    fn module_is_class_path_only(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// `exports` directives declared by `module_name`, as
    /// `(package, targets)`. Package names are INTERNAL (slash) form; an empty
    /// `targets` is an unqualified export.
    fn module_exports(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        let _ = module_name;
        vec![]
    }

    /// `opens` directives declared by `module_name`, same shape as
    /// [`module_exports`].
    fn module_opens(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        let _ = module_name;
        vec![]
    }

    /// `requires` directives declared by `module_name`, as
    /// `(module name, is_transitive, is_static)`.
    fn module_requires(&self, module_name: &str) -> Vec<(String, bool, bool)> {
        let _ = module_name;
        vec![]
    }

    /// `provides` directives declared by `module_name`, as
    /// `(service, providers)` — binary class names in INTERNAL (slash) form.
    fn module_provides(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        let _ = module_name;
        vec![]
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

    /// Is any currently-LOADED class a member of `package_slash` (immediate
    /// members only; `""` is the default package)?
    ///
    /// The question `ClassLoader.getDefinedPackage` actually asks. A loader
    /// DEFINES a package once it has defined a class in it, which is why the
    /// class-file *visibility* probes above cannot answer it: the boot image
    /// contains `java/awt/image/*.class` on every run, and HotSpot still
    /// answers `null` for `java.awt.image` until something loads one.
    ///
    /// # The default is `false`, and that is the safe direction
    ///
    /// An implementation that has not overridden this answers "no class of
    /// that package is loaded", which makes the module-backed arm of
    /// `classloader::builtin_loader_defines_package` decline rather than
    /// fabricate. Declining is the answer `getDefinedPackage`'s contract
    /// prefers (a missing `Package` over an invented one) and is exactly what
    /// this VM did before that arm existed.
    fn any_loaded_class_in_package(&self, package_slash: &str) -> bool {
        let _ = package_slash;
        false
    }

    /// [`Self::any_loaded_class_in_package`], narrowed to the classes ONE
    /// loader defined. `loader_id` is the flat `ClassLoaderId` numbering
    /// [`Self::loader_id_of_class`] reports.
    ///
    /// Same `false` default, for the same reason: an implementation that has
    /// not overridden this declines rather than fabricating.
    fn any_loaded_class_in_package_for_loader(&self, package_slash: &str, loader_id: u32) -> bool {
        let _ = (package_slash, loader_id);
        false
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

    /// True while the current thread is executing inside some class's
    /// `<clinit>` (including nested/re-entrant `<clinit>` calls it
    /// transitively triggers).
    ///
    /// Callers that would otherwise force a *different*, unrelated class's
    /// full initialization as a side effect of reflection (e.g. resolving
    /// the return type of a `java.lang.reflect.Method` mirror) must check
    /// this first and skip the eager `initialize_class` call when true --
    /// JVMS §5.5 never requires initializing a class merely because its
    /// name shows up in another class's method signature, and doing so
    /// anyway while genuinely mid-`<clinit>` lets the forced class observe
    /// the in-progress class's statics before they are assigned. Defaults
    /// to `false` (preserves prior behavior) for implementors that do not
    /// track this.
    fn in_clinit(&self) -> bool {
        false
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

    /// Is the package declaring `target_class_id` *exported* (not opened) to
    /// the module of `accessor_class_id`?
    ///
    /// This is the narrower JPMS question `AccessibleObject
    /// .checkCanSetAccessible` asks once the `opens` test has failed. JDK 17+
    /// still permits `setAccessible(true)` on a **public** member of a
    /// **public** class when the declaring package is merely *exported* — no
    /// `--add-opens` needed. An `opens`-only gate therefore over-denies in
    /// exactly the place HotSpot allows, which is why this second query
    /// exists rather than being folded into the one above.
    ///
    /// Default `false` (fail closed): it is only ever consulted to widen a
    /// decision [`check_deep_reflection_access`] has already refused, so a
    /// mock that does not implement it simply keeps that refusal.
    ///
    /// [`check_deep_reflection_access`]: NativeContext::check_deep_reflection_access
    fn reflective_export_to_accessor(
        &self,
        _accessor_class_id: ClassId,
        _target_class_id: ClassId,
    ) -> bool {
        false
    }

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

    /// [`Self::invoke_special`] for a caller that ALREADY holds the declaring
    /// class's resolved `ClassId`.
    ///
    /// Same rationale as [`Self::invoke_by_class_id`] (see its doc comment for
    /// the full argument), applied to the invokespecial dispatch contract.
    /// `invoke_special` resolves `class_name` through the loader-blind global
    /// lookup, which collapses to ONE copy per binary name - so a caller that
    /// knows the exact declaring class must not throw that identity away.
    ///
    /// The concrete bug this exists for: `Method.invoke` routes a *private* or
    /// *cross-package package-private* instance method through
    /// `invoke_special` (both must bypass virtual dispatch - JLS 8.4.8.1).
    /// The `Method` mirror's own `clazz` slot already names the exact declaring
    /// class, but the name-based re-resolution picked the APPLICATION-loader
    /// copy whenever an isolating loader (Spring Boot's
    /// `ModifiedClassPathClassLoader` under `@ForkedClassPath`) had defined its
    /// own copy of that class. See servletcontextlistener-forkedclasspath-mockito-notamock-FIXED.md.
    ///
    /// Default implementation falls back to the name-based
    /// [`Self::invoke_special`] for contexts with no ClassId fast path.
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

/// The descriptor byte that means "do not decode this slot, hand me the bits".
///
/// A JVM field descriptor's first byte is always one of `B C D F I J S Z L [`
/// (JVMS §4.3.2). `0` is none of them and cannot be produced by a well-formed
/// class file, so it is unambiguous as a sentinel.
///
/// It is load-bearing for [`NativeHeapAccess::get_field_raw`]: the descriptor
/// -aware decode in `gc::heap::coerce_field_value_for_slot` is a `match` on
/// this byte whose final arm is `_ => value`, so an unrecognised byte returns
/// the slot verbatim AND — because every coercion-loss report lives inside one
/// of the recognised arms — reports nothing to the G30 instrument. If a future
/// arm is ever added for `0`, `get_field_raw` silently stops being raw; that is
/// why the byte is named here rather than spelled inline at the call, and why
/// `a_raw_read_uses_a_byte_that_is_not_a_jvm_descriptor` pins it.
pub const RAW_SLOT_DESCRIPTOR: u8 = 0;

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
    /// wildfly-parallel-boot-stale-objectref-residual.md.
    ///
    /// # A funnel that allocates takes its receiver by `&mut ObjectRef`
    ///
    /// Pinning protects the code that does it. It does NOT protect a CALLER
    /// that passed the receiver in by value: the callee's refreshed reference
    /// dies at its closing brace and the caller keeps the pre-move address.
    /// That is a whole defect family rather than a one-off —
    /// `WORKER-5-NOTE-10` traced `TreeMap.size()` answering **0** on a view
    /// nothing had mutated to exactly this, and `WORKER-5-NOTE-13` converted
    /// 23 more funnels across all seven native crates.
    ///
    /// So the convention is:
    ///
    /// > **A helper that (a) takes a receiver `ObjectRef`, (b) can allocate,
    /// > and (c) returns no reference MUST take that receiver as
    /// > `&mut ObjectRef` and write the refreshed value back.**
    ///
    /// ```ignore
    /// fn refresh_the_thing(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    ///     let pin = ctx.pin_native_root(*this);
    ///     let out = refresh_the_thing_body(ctx, *this);
    ///     *this = ctx.read_native_pin(pin, *this);   // hand the caller the new address
    ///     ctx.unpin_native_roots(pin);
    ///     out
    /// }
    /// ```
    ///
    /// `&mut` is the point, not the pin: it turns every unconverted caller
    /// into a COMPILE ERROR. Returning the reference instead is equally sound
    /// but weaker — a plain `-> ObjectRef` can be dropped in silence. Pinning
    /// at each call site is weakest of all: it is what the next audit has to
    /// find again, and of the call sites `WORKER-5-NOTE-12`/`-13` examined,
    /// the ones that had been protected by hand were consistently outnumbered
    /// by the ones beside them that had not.
    ///
    /// `scripts/stale-receiver-audit.py` is the gate; its baseline is empty.
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
    fn hashmap_string_node_cache_put(
        &mut self,
        _map: ObjectRef,
        _key_object: ObjectRef,
        _key: &str,
        _node: ObjectRef,
    ) {
    }

    /// Look up a ConcurrentHashMap segment node whose mutation generation is
    /// still current. The segment layout has no Java `modCount` field, so it
    /// cannot share the exact-HashMap validity predicate above.
    fn chm_string_node_cache_get_object(
        &mut self,
        _map: ObjectRef,
        _key: ObjectRef,
    ) -> Option<(i32, u64, Value)> {
        None
    }

    /// Publish a ConcurrentHashMap segment node guarded by the caller's
    /// seqlock-style mutation generation.
    fn chm_string_node_cache_put(
        &mut self,
        _map: ObjectRef,
        _segment_id: i32,
        _key_object: ObjectRef,
        _key: &str,
        _node: ObjectRef,
        _generation: u64,
    ) {
    }

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

    /// [`get_field`](Self::get_field) for a caller that already knows the
    /// field's declared descriptor byte (`I`, `J`, `Z`, `L`, ...).
    ///
    /// `get_field` has to ask the class metadata what the slot's declared type
    /// is before it can decode the raw storage — a per-read lookup that a
    /// native which resolved its field indices once already has the answer to.
    /// Passing the descriptor in skips that lookup; passing a WRONG one decodes
    /// the slot as the wrong type, exactly as if the class had declared it that
    /// way, so only use it with a descriptor read out of the same class
    /// metadata the index came from.
    ///
    /// The default implementation ignores the hint and delegates, so an
    /// implementor with no cheaper path needs to do nothing.
    fn get_field_typed(&self, obj: ObjectRef, index: usize, descriptor: u8) -> Value {
        let _ = descriptor;
        self.get_field(obj, index)
    }

    /// [`get_field_typed`](Self::get_field_typed) for several slots of ONE
    /// object at once.
    ///
    /// Each element of `slots` is `(slot index, descriptor byte)`; results land
    /// in `out` at the matching position, and the shorter of the two bounds the
    /// batch.
    ///
    /// Every single-field read has to canonicalise the reference first (an
    /// object may have been evacuated since the caller obtained it). A native
    /// that needs three or four fields of the same receiver per call — which is
    /// what the per-element `java.nio.DirectByteBuffer` accessors do, once per
    /// byte moved — pays that, and the dynamic dispatch, once instead of once
    /// per field.
    fn get_fields_typed(&self, obj: ObjectRef, slots: &[(usize, u8)], out: &mut [Value]) {
        for (slot, dst) in slots.iter().zip(out.iter_mut()) {
            *dst = self.get_field_typed(obj, slot.0, slot.1);
        }
    }

    /// Write an object field by slot index.
    ///
    /// `index` is an absolute heap field slot (see [`FieldMetadata::slot_index`]).
    /// The caller must ensure it is in range for `obj`'s class; the
    /// implementation MUST bounds-check and MUST NOT write out of range (M4a).
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value);

    /// Read an object field by slot index **without descriptor coercion** —
    /// the `Value` exactly as it is stored, tag included.
    ///
    /// # Why this exists (G52-1 NOMINATION 1, G56-1)
    ///
    /// Every other slot-indexed accessor on this trait resolves the field's
    /// declared descriptor and routes the slot through
    /// `gc::heap::coerce_field_value_for_slot`: [`get_field`](Self::get_field),
    /// [`get_field_volatile`](Self::get_field_volatile),
    /// [`compare_and_swap_field`](Self::compare_and_swap_field), and
    /// [`get_field_typed`](Self::get_field_typed) with a real descriptor.
    /// The only non-coercing pair was
    /// [`get_field_by_name`](Self::get_field_by_name) /
    /// [`set_field_by_name`](Self::set_field_by_name), which is **name**-keyed:
    /// it cannot be driven from a slot index at all, and it resolves a shadowed
    /// field name to the wrong slot. So a native that holds a slot index and
    /// wants the stored bits — `Object.clone()`, which the JVM specifies as a
    /// verbatim field copy, and any reader that must tell "never written" from
    /// "explicitly null" — had no way to ask for them. `Object.clone()` did not
    /// opt into coercion; the API moved underneath it.
    ///
    /// # Contract
    ///
    /// An implementation MUST return the slot's stored `Value` unchanged, and
    /// MUST still bounds-check `index` and fail safe exactly as
    /// [`get_field`](Self::get_field) does (M4a). "Raw" licenses skipping the
    /// descriptor decode; it never licenses an out-of-range access. The
    /// reference must still be canonicalised (forwarded) before the read — a
    /// raw read of a stale address is not a raw read of the object.
    ///
    /// # The default implementation is already raw, on both impls that exist
    ///
    /// It routes through [`get_field_typed`](Self::get_field_typed) with
    /// [`RAW_SLOT_DESCRIPTOR`], which is not a JVM field-descriptor first byte.
    ///
    /// * Production (`NativeContextImpl` in the `vm` crate) overrides
    ///   `get_field_typed` as `heap.get_field_as(obj, index, descriptor)`, and
    ///   `coerce_field_value_for_slot` dispatches on the descriptor byte with a
    ///   final `_ => value` arm. A byte outside `J D F I B C S Z L [` therefore
    ///   returns the slot verbatim and — this is the half that matters for the
    ///   G30 instrument — fires **no** coercion-loss event. It is also cheaper
    ///   than [`get_field`](Self::get_field): the descriptor is supplied, so
    ///   `resolve_field_descriptor_byte_cached` is skipped entirely.
    /// * Mocks that do not override `get_field_typed` get this trait's default,
    ///   which ignores the byte and calls `get_field` — already raw there.
    ///
    /// An implementor with a cheaper direct route should override this; one
    /// with none needs to do nothing.
    fn get_field_raw(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field_typed(obj, index, RAW_SLOT_DESCRIPTOR)
    }

    /// Write an object field by slot index **without descriptor coercion** —
    /// the `Value` is stored with the tag the caller handed over.
    ///
    /// The store half of [`get_field_raw`](Self::get_field_raw). See that
    /// method for why the pair exists; the contract is the same, and an
    /// implementation MUST still bounds-check `index` and MUST NOT write out
    /// of range (M4a).
    ///
    /// # THIS DEFAULT IS NOT RAW. Read before pairing it with a raw read.
    ///
    /// There is no typed setter on this trait to lean on the way
    /// [`get_field_raw`](Self::get_field_raw) leans on
    /// [`get_field_typed`](Self::get_field_typed), and the only non-coercing
    /// setter reachable from here — [`set_field_by_name`](Self::set_field_by_name)
    /// — is name-keyed and cannot be driven from a slot index. The default
    /// therefore delegates to [`set_field`](Self::set_field), which in the
    /// production `NativeContextImpl` resolves the descriptor and coerces.
    /// Overriding it there is a one-line body — `heap.set_field(obj, index,
    /// value)` after the usual `load_and_forward` — and until that lands this
    /// method is raw only on impls whose `set_field` was already raw (the test
    /// mocks).
    ///
    /// **Do not pair a raw read with this default in a copy loop.** That
    /// combination is strictly worse for the G30 instrument than coercing
    /// both halves: a `read` of `Int(0)` at an `L` slot currently answers
    /// `Object(None)` and is counted in the benign `read` column, whereas a
    /// raw read followed by a coercing store hands the `Int(0)` to the setter
    /// and re-reports it as a **`store`** — the column `gc/src/collector.rs`
    /// reserves for real defects ("a read that coerces is usually the slot
    /// repairing a never-initialised tag, a store that coerces has destroyed
    /// something a writer meant"). G52-1 §1.5 measured that migration at ~24
    /// events for `native_object_clone` alone. The verbatim-copy callers must
    /// wait for both halves to be genuinely raw and then switch together.
    fn set_field_raw(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value);
    }

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

    /// Reclaim the heap and report whether a retry is worth making, for a
    /// native whose `try_new_array` / `try_new_ref_array` / `alloc_object`
    /// just returned `None`.
    ///
    /// # Why this is not simply done inside the allocators
    ///
    /// The fallible native allocators deliberately do NOT collect. A native
    /// holds raw `ObjectRef`s in Rust locals, and those are in no GC root set:
    /// a collection triggered underneath one would relocate them (dangling the
    /// locals) or sweep them (freeing live objects). See `runtime::native_oom`,
    /// whose whole design follows from that rule. So the allocators get exactly
    /// one attempt and then report failure — which is correct for them and, on
    /// its own, wrong for the program.
    ///
    /// What the rule costs, measured: the interpreter's `gc_alloc_array` and
    /// the JIT's `jit_newarray` both run a LADDER on a failed allocation —
    /// retire the TLAB, force a collection, retry, `last_ditch_reclaim`, retry
    /// again — and only then throw. A native allocating the same array gets no
    /// ladder at all, so it reports `OutOfMemoryError` on the first refusal.
    /// On H2 `TestBenchmark` (`-Xmx1g`, ZGC) that surfaced as a 10 MiB
    /// `ByteBuffer.allocate` failing with the heap **97% free**: the arena had
    /// no hole that big at that instant, the collection that would have opened
    /// one had not been asked for, and repeating the identical allocation one
    /// Java statement later succeeded immediately.
    ///
    /// # The precondition, which the CALLER owns
    ///
    /// Only call this when **this native holds no unpinned `ObjectRef` in a
    /// Rust local** — i.e. at an allocation performed before the native has
    /// acquired any heap reference, or with everything it holds pinned through
    /// [`pin_native_root`](Self::pin_native_root). At that point the collection
    /// is exactly as safe as the one the interpreter runs between two
    /// bytecodes. A native that has already stashed a bare `ObjectRef` must NOT
    /// call this; it must report OOM as before.
    ///
    /// That is why this is a separate call rather than a retry folded into the
    /// allocators: the allocators cannot see their caller's locals, and the
    /// caller can.
    ///
    /// Returns `false` when no reclamation was attempted or the heap is
    /// GC-thrashing past the overhead limit — in which case the caller should
    /// surface `OutOfMemoryError` without a retry, rather than spin. The
    /// default is `false` so mock/non-VM contexts keep their current
    /// single-attempt behaviour.
    fn reclaim_before_alloc_retry(&mut self) -> bool {
        false
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

    /// Bulk read from a Java `int[]` array into a host `i32` buffer.
    ///
    /// Default: per-element loop. VM override: one `copy_nonoverlapping` of
    /// `n * 4` bytes from the compact int-array payload. Added for the
    /// BouncyCastle digest kernels, whose whole reason for existing as a
    /// native is that the per-element `get_array_element` path costs a virtual
    /// dispatch plus a `Value` box per word — 16 of them per compression
    /// block, which is most of what the intrinsic was meant to remove.
    fn read_int_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [i32]) -> usize {
        // Same out-of-bounds guard as `read_byte_array_into`.
        let src_len = self.array_length(arr);
        if src_off > src_len {
            return 0;
        }
        let available = src_len - src_off;
        let n = available.min(dst.len());
        for i in 0..n {
            match self.get_array_element(arr, src_off + i) {
                Value::Int(v) => dst[i] = v,
                _ => return i,
            }
        }
        n
    }

    /// Bulk write from a host `i32` buffer into a Java `int[]` array at the
    /// given destination offset. Returns `true` on success, `false` on bounds
    /// error / wrong array kind. Symmetric to [`read_int_array_into`](Self::read_int_array_into).
    fn write_int_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[i32]) -> bool {
        // Bounds-check the destination before any writes so we never overflow
        // past the array end while reporting `true` (the CRIT fix the byte and
        // char twins above carry).
        let dst_len = self.array_length(arr);
        if dst_off
            .checked_add(src.len())
            .map_or(true, |end| end > dst_len)
        {
            return false;
        }
        for (i, v) in src.iter().enumerate() {
            self.set_array_element(arr, dst_off + i, Value::Int(*v));
        }
        true
    }

    /// Bulk write from a host `u16` buffer into a Java `char[]` array at
    /// the given destination offset. Returns `true` on success, `false` on
    /// bounds error / wrong array kind.
    ///
    /// Copy `src` into a primitive array's payload as raw bytes.
    ///
    /// Returns `false` on a bounds error, a non-array receiver, a
    /// reference array, or a context with no bulk path.
    ///
    /// # Why a byte-level API rather than one per element type
    ///
    /// The typed helpers above cover `byte`, `char` and `int`, which is
    /// where they were needed. Adding `float`, `double`, `long` and
    /// `short` would be four more near-identical methods here and four
    /// more overrides in the VM, all doing the same `copy_nonoverlapping`
    /// once the width is known.
    ///
    /// The caller that needs them — GPU array marshalling — already holds
    /// its data as a flat `Vec<u8>` in native-endian order, because that
    /// is what a device buffer is. So the byte form is not a workaround
    /// for it; it is the shape it actually wants, and one pair of methods
    /// serves every primitive width.
    ///
    /// Native-endian, matching the `to_ne_bytes` the callers already use:
    /// host and heap are the same machine, so this is a copy and not a
    /// conversion.
    ///
    /// Reference arrays are rejected outright. Writing raw bytes over
    /// object references would hand the collector pointers it never
    /// issued.
    fn write_primitive_array_bytes(
        &mut self,
        _arr: ObjectRef,
        _byte_off: usize,
        _src: &[u8],
    ) -> bool {
        false
    }

    /// Read a primitive array's payload as raw bytes into `dst`.
    ///
    /// Returns the number of bytes copied, or `0` on any of the
    /// conditions [`write_primitive_array_bytes`](Self::write_primitive_array_bytes)
    /// rejects. Zero is unambiguous here: a caller asking for zero bytes
    /// has nothing to do either way.
    fn read_primitive_array_bytes(
        &self,
        _arr: ObjectRef,
        _byte_off: usize,
        _dst: &mut [u8],
    ) -> usize {
        0
    }

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
        _locale: Option<ObjectRef>,
        _upper: bool,
    ) -> Option<ObjectRef> {
        None
    }

    /// Create and retain the alternating pair for an ASCII case conversion.
    fn create_ascii_case_string_cached(
        &mut self,
        _source: ObjectRef,
        _locale: Option<ObjectRef>,
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
    ///
    /// # This IS the lossless String writer — there is no missing primitive
    ///
    /// `create_string`/`create_string_uninterned`/`create_string_uninterned_gc_safe`
    /// all take a `&str`, and a Rust `str` cannot hold an unpaired UTF-16
    /// surrogate — so a native that reads a Java `String` losslessly and then
    /// hands the units to any of those has thrown the surrogate away at the
    /// LAST step. The recurring conclusion from that is "`NativeContext` needs
    /// a `create_string_from_utf16`". It does not: `new_object("java/lang/
    /// String")` followed by this method is that constructor, it is what the
    /// `String(char[])` / `String(char[], int, int)` overrides already use, and
    /// on the VM it lands in the same `populate_java_string_fields` that
    /// `create_java_string_from_units` uses — the one place that decides a
    /// compact `String`'s coder and byte order.
    ///
    /// `native-builtins`' `lang_string::sb_string_from_units` packages the pair
    /// (plus the GC pin and the well-formed fast path) into a single call, and
    /// is what every `String`-returning native in that file should end in.
    ///
    /// A third spelling of the same construction would be a second encoding of
    /// one concept, reconciled only where the bytes are consumed. Extend or
    /// re-route this one instead.
    fn init_string_from_units(&mut self, this: ObjectRef, units: &[u16]) -> bool {
        let arr = self.new_array(ArrayElementType::Char, units.len());
        for (i, &u) in units.iter().enumerate() {
            self.set_array_element(arr, i, Value::Int(u as i32));
        }
        self.set_field(this, 0, Value::Object(Some(arr)));
        true
    }

    /// Read a Java String object back to a Rust String.
    ///
    /// LOSSY BY CONSTRUCTION for one input: a Rust `String` cannot hold an
    /// unpaired UTF-16 surrogate, so any text containing one comes back with
    /// U+FFFD substituted. That is fine wherever the result is only inspected
    /// (a class name, a charset name, a flag) and wrong wherever it is handed
    /// back to Java. Use [`read_string_units`] there.
    fn read_string(&self, obj: ObjectRef) -> Option<String>;

    /// Read a Java String object as UTF-16 code units, losing nothing.
    ///
    /// The units-exact counterpart of [`read_string`], and the fourth member of
    /// the units-exact family beside [`read_char_array_into`],
    /// [`write_char_array_from`] and [`init_string_from_units`]. `G55-1` N3
    /// nominated it because `native-collections` cannot decode a String itself
    /// -- it does not depend on `native-builtins`, and a local decoder would be
    /// a second encoding of the compact-string layout.
    ///
    /// The default is deliberately LOSSY -- it is `read_string` widened -- so
    /// that no implementation regresses by not overriding it, and so the
    /// surrogate hazard stays exactly where it already was for anyone who does
    /// not. The VM overrides it with the real reader.
    fn read_string_units(&self, obj: ObjectRef) -> Option<Vec<u16>> {
        self.read_string(obj).map(|s| s.encode_utf16().collect())
    }

    /// Build a fresh Java String from UTF-16 code units, losing nothing.
    ///
    /// The write half of [`read_string_units`], and the reason a units-exact
    /// READER alone would not have been enough: a value read without loss and
    /// then written back through [`create_string`] is lossy again at the last
    /// step. NOT [`init_string_from_units`], which fills an ALREADY-ALLOCATED
    /// receiver and assumes the `char[]` layout -- a real JDK `String` is
    /// `byte[]` plus a coder, so that default is wrong for the mode this
    /// matters most in.
    ///
    /// Default is lossy for the same reason as above.
    fn create_string_from_units(&mut self, units: &[u16]) -> ObjectRef {
        let text = String::from_utf16_lossy(units);
        self.create_string(&text)
    }

    /// Return the raw Java `String.hashCode()` for a confirmed String object.
    /// `None` means "no hash was computed": `obj` is not a String, or an
    /// overriding implementation could not read its character storage.
    /// Implementations may override this to inspect compact storage without
    /// allocating a host String.
    fn java_string_hash_code(&self, obj: ObjectRef) -> Option<i32> {
        self.read_string(obj).map(|text| {
            text.encode_utf16().fold(0i32, |hash, unit| {
                hash.wrapping_mul(31).wrapping_add(unit as i32)
            })
        })
    }

    /// Compare two confirmed Java Strings without routing through Java
    /// dispatch.
    ///
    /// `Some(_)` is an answer. `None` means **the comparison was not made** —
    /// an operand is not a String, or an overriding implementation could not
    /// read one operand's character storage — and the caller must fall back to
    /// dispatching `String.equals`. An implementation must never report `false`
    /// for a pair it did not actually read: doing so silently turned every
    /// `ConcurrentHashMap.get` on a String key into a miss (see
    /// `chm-get-misses-stored-key-in-process-RETIRED-20260804.md`).
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

    /// Bytes of the Java heap that are **currently occupied by live objects**
    /// — the "used" half of `Runtime.freeMemory()`, `MemoryUsage.getUsed()`
    /// and every other heap-occupancy report.
    ///
    /// Occupancy, not a high-water mark. That distinction is the whole content
    /// of this doc comment, because a bump-pointer arena makes the two easy to
    /// confuse and the VM answered with the wrong one until 2026-09-08: the
    /// generational young sweep reclaims in place, into a free list, WITHOUT
    /// retreating the arena cursor, so the raw cursor stays pinned at its
    /// high-water mark for the rest of the process. Programs that size caches
    /// or buffers from the free heap then see a heap that never empties: H2's
    /// `TestValueMemory` is `totalMemory() - freeMemory()` around a pair of
    /// `System.gc()` calls, and on the generational arm it read 6715 KB against
    /// a 2928 KB threshold where occupancy is 2228.
    fn heap_allocated_bytes(&self) -> usize;

    /// Cumulative bytes the **calling** thread has allocated since it started,
    /// or `None` when the VM cannot account for it.
    ///
    /// This is the source for `com.sun.management.ThreadMXBean
    /// .getCurrentThreadAllocatedBytes` / `getThreadAllocatedBytes(long)`.
    /// `None` is the honest answer a mock or a thread-less context gives, and
    /// the bean turns it into the JMM's documented `-1` plus
    /// `isThreadAllocatedMemorySupported() == false` — a "not supported" that
    /// callers already handle, rather than a fabricated number.
    ///
    /// The VM implementation reads `Tlab::thread_allocated_bytes`, which is
    /// live-cursor based and therefore sees compiled code's inline allocation
    /// as well as the interpreter's.
    fn current_thread_allocated_bytes(&self) -> Option<u64> {
        None
    }

    /// Cumulative bytes allocated by ALL threads over the life of the process,
    /// for `com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes`.
    ///
    /// Must never decrease. Deliberately NOT
    /// [`Self::heap_allocated_bytes`], which is an occupancy gauge and falls at
    /// every collection — a caller measuring a window containing a GC would get
    /// the difference of two occupancies rather than what it allocated.
    fn total_allocated_bytes(&self) -> Option<u64> {
        None
    }

    /// Bytes currently COMMITTED for the Java heap — backing storage the VM
    /// holds whether or not anything lives in it. `Runtime.totalMemory()`, the
    /// JMX heap `MemoryUsage.getCommitted()`, and (minus
    /// [`Self::heap_allocated_bytes`]) `Runtime.freeMemory()`.
    ///
    /// Capacity, never occupancy: `totalMemory()` is expected to move only when
    /// the heap grows or shrinks. See `gc::vm_heap::VmHeap::committed_bytes`.
    ///
    /// The default is the historical `Runtime.totalMemory()` stub, kept for
    /// mock/test contexts that have no heap; the VM overrides it.
    fn committed_heap_bytes(&self) -> usize {
        64 * 1024 * 1024
    }

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

    /// Notify the GC's reference processor that a `Reference.enqueue()` call
    /// just enqueued this reference itself (the application's own explicit
    /// enqueue, as opposed to the GC discovering the referent dead).
    ///
    /// PGJDBC-PHANTOM-GHOST (2026-08-07): without this, a `Reference` that the
    /// application manually retires while its referent is STILL reachable
    /// (e.g. pgjdbc's `SimpleQuery.unprepare()`/`setCleanupRef()`, which
    /// `clear()`s and `enqueue()`s the *previous* `PhantomReference` when a
    /// long-lived, reused `SimpleQuery` gets re-prepared) leaves a stale
    /// `enqueued: false` bookkeeping entry in the GC's registry. If that same
    /// referent is later shared with a NEW Reference (as in the pgjdbc
    /// pattern) and eventually dies for real, the GC's own weak/phantom
    /// processing rediscovers the stale entry and delivers it a SECOND time —
    /// a ghost the application already fully drained and forgot, so its own
    /// removal bookkeeping (e.g. a `HashMap.remove(ref)`) returns null. See
    /// `ReferenceProcessor::mark_manually_enqueued`'s doc for the full
    /// mechanism. The VM overrides this to retire the registry entry so it is
    /// never rediscovered; the default no-op keeps mock/test contexts
    /// compiling.
    fn mark_reference_manually_enqueued(&mut self, _reference_obj: ObjectRef) {}
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
    /// `wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`).
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
    /// `wildfly-parallel-boot-stale-objectref-residual.md`)
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

    /// The OS thread id backing a Java `Thread` mirror, or `None` if unknown.
    ///
    /// Enables ARBITRARY-thread CPU time (`ThreadMXBean.getThreadCpuTime(long)`
    /// and therefore `isThreadCpuTimeSupported()`), which reported `-1`/false
    /// only because `ThreadRegistry`'s per-thread `os_tid` — published since
    /// forever by `set_os_tid_current` — was not reachable from a native.
    /// Current-thread CPU time never needed this and was already real.
    fn thread_os_tid(&self, _thread_obj: ObjectRef) -> Option<u32> {
        None
    }

    /// Total JIT compilation time in milliseconds, or `None` when the VM keeps
    /// no such accounting.
    ///
    /// Backs BOTH `CompilationMXBean.getTotalCompilationTime()` and
    /// `isCompilationTimeMonitoringSupported()` — returning one value for both
    /// is deliberate, so they cannot drift into claiming support for a number
    /// that is not kept. The unit is already milliseconds in
    /// `jit::tiered::CompilationStats::total_compile_time_ms`, which is also
    /// the unit the JMM specifies.
    fn jit_total_compile_time_ms(&self) -> Option<u64> {
        None
    }

    /// Virtual-thread scheduler counters as `(pool_size, mounted, queued)`,
    /// or `None` when no scheduler is running.
    ///
    /// Returned as ONE triple rather than three accessors so
    /// `VirtualThreadSchedulerMXBean`'s three getters cannot sample the
    /// scheduler at three different instants and report a mutually
    /// inconsistent snapshot.
    fn vt_scheduler_stats(&self) -> Option<(i32, i32, i64)> {
        None
    }

    /// Number of objects queued for finalization but not yet finalized —
    /// `MemoryMXBean.getObjectPendingFinalizationCount()`.
    ///
    /// The datum has always existed (`ReferenceProcessor::finalization_queue`,
    /// drained by a real `FinalizerThread`); it simply was not reachable from
    /// `native-builtins`, so the native returned a flat 0. Zero is a
    /// legitimate ANSWER but was not a legitimate CONSTANT: it reported "no
    /// finalization backlog" even while the queue was growing, which is
    /// exactly the condition an operator queries this bean to detect.
    ///
    /// Defaults to 0 for implementations with no reference processor.
    fn pending_finalization_count(&self) -> i32 {
        0
    }

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

    /// Dispatch a built-in GEMM: `C[MxN] = A[MxK] * B[KxN]`, row-major.
    ///
    /// `half` selects the fp16 input variant (fp32 accumulate) over the
    /// all-fp32 one. The three handles are `craton.gpu.GpuArray` handles,
    /// not Java arrays, deliberately: their device buffers are cached, so
    /// a weight matrix uploaded once stays resident across calls. A
    /// decode step multiplies by the same weights every token, and
    /// re-uploading them would cost more than the arithmetic.
    ///
    /// `_trans_a` / `_trans_b` read the corresponding operand transposed,
    /// expressed as a stride pair rather than a separate kernel, so the
    /// operand's element count is unchanged and only its indexing differs.
    ///
    /// `_stream_handle` places the launch on a Java-visible `GpuStream`;
    /// `None` uses the shared built-in stream.
    ///
    /// Returns a submission handle with the usual lifecycle, or `None`
    /// when this VM has no GPU offload compiled in.
    ///
    /// This exists because the bytecode lowering cannot express a matrix
    /// multiply — three nested loops, a shared tile, a barrier — and
    /// rejects such a method rather than mis-lowering it. See
    /// `vm::runtime::kernels`.
    #[allow(clippy::too_many_arguments)]
    fn gpu_dispatch_gemm(
        &mut self,
        _half: bool,
        _a_handle: u64,
        _b_handle: u64,
        _c_handle: u64,
        _m: i32,
        _n: i32,
        _k: i32,
        _trans_a: bool,
        _trans_b: bool,
        _stream_handle: Option<u64>,
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

    /// Begin recording dispatches on `stream_handle` into a graph instead
    /// of running them.
    ///
    /// A launch costs the host whether or not the device is busy, and a
    /// loop issuing hundreds of small launches per unit of work pays that
    /// hundreds of times. Recording them once and replaying the recording
    /// removes the per-launch host cost entirely.
    ///
    /// Default impl answers `false` (no GPU offload). The VM override
    /// calls `runtime::offload::OffloadCache::graph_begin_capture`.
    fn gpu_graph_begin_capture(&mut self, _stream_handle: u64) -> bool {
        false
    }

    /// Stop recording and instantiate. Answers a graph handle, or `0` when
    /// there was no capture or it was invalidated.
    fn gpu_graph_end_capture(&mut self, _stream_handle: u64) -> u64 {
        0
    }

    /// Submit every launch a graph recorded, onto `stream_handle`.
    ///
    /// Answers a submission handle, awaited and released exactly like a
    /// dispatch's, or `0` if the replay was refused. A replay is
    /// asynchronous; a caller that reads a result without awaiting the
    /// handle reads what was there before.
    fn gpu_graph_replay(&mut self, _stream_handle: u64, _graph_handle: u64) -> u64 {
        0
    }

    /// Open an argument-update pass over a captured graph: the caller
    /// re-issues its dispatch sequence and each dispatch rewrites the
    /// arguments of the node it corresponds to, instead of launching.
    fn gpu_graph_begin_replay(&mut self, _stream_handle: u64, _graph_handle: u64) -> bool {
        false
    }

    /// Close the pass and submit the graph once. Answers a submission
    /// handle, or `0` if the caller's sequence did not match the one
    /// that was captured.
    fn gpu_graph_end_replay(&mut self, _stream_handle: u64) -> u64 {
        0
    }

    /// How many nodes a graph holds, or `-1` for an unknown handle. The
    /// count a caller checks against the dispatches it made while
    /// capturing: a graph with fewer nodes replays successfully and does
    /// less.
    fn gpu_graph_node_count(&self, _graph_handle: u64) -> i32 {
        -1
    }

    /// Free a graph. Idempotent, like every other release in this trait.
    fn gpu_graph_release(&mut self, _graph_handle: u64) {}

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

    /// Why the submission at `handle` failed, as recorded at the point of
    /// failure. Returns `None` when the handle is not in the real
    /// registry or the submission did not fail.
    ///
    /// Backs `Native.futureErrorKind`. Without it the Java side can only
    /// guess the `GpuException` subclass from the driver's message text.
    ///
    /// Default impl returns `None` (no GPU offload).
    fn gpu_future_error_kind(&self, _handle: u64) -> Option<GpuErrorKind> {
        None
    }

    /// Block until the submission at `handle` completes or
    /// `timeout_nanos` elapses.
    ///
    /// Returns one of [`GPU_AWAIT_COMPLETED`], [`GPU_AWAIT_TIMED_OUT`], or
    /// [`GPU_AWAIT_UNSUPPORTED`]. The default is `GPU_AWAIT_UNSUPPORTED`,
    /// which tells the caller to fall back to the polling path it already
    /// has — so a VM without this override behaves exactly as before.
    ///
    /// The point of implementing it here rather than in Java is that the
    /// Java fallback costs one native crossing per poll and floors its
    /// latency at the poll interval. A native implementation waits on the
    /// device's own completion signal.
    fn gpu_future_await(&self, _handle: u64, _timeout_nanos: u64) -> i32 {
        GPU_AWAIT_UNSUPPORTED
    }

    /// Phase 8 #1 — evict the device-side buffer cache entry for
    /// the given `GpuArray` handle. Called by
    /// `Native.releaseArray` so a long-running Java program that
    /// churns through GpuArrays doesn't accumulate device memory.
    ///
    /// Default impl is a no-op (no GPU offload). The VM override
    /// calls `runtime::offload::device_cache::release(handle)`.
    /// Drop the offload runtime's registry entry for one async
    /// submission handle.
    ///
    /// The handle is the same one `gpu_future_synchronize` /
    /// `gpu_future_await` take: the Java "future handle" IS the offload
    /// submission handle. Default no-op so a host without the offload
    /// runtime (or a `gpu-offload`-less build) needs no arm.
    ///
    /// Added 2026-09-02. `offload::SUBMISSIONS` had one insert and one
    /// remove, and the remove had no production caller -- no Java
    /// program, however correctly written, could drain the registry,
    /// because `Native.releaseFuture` only removed from
    /// `native-builtins`' own `state::futures` map. This is the missing
    /// half of that path.
    fn gpu_release_submission(&mut self, _handle: u64) {}

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
    /// Write little-endian host `bytes` into the device buffer that
    /// backs `handle`, keeping its device pointer.
    ///
    /// The upload counterpart of
    /// [`gpu_array_download_if_dirty`](Self::gpu_array_download_if_dirty),
    /// and the one thing a captured CUDA graph needs from the host
    /// between replays: a graph node holds the pointer it was captured
    /// with, so new input has to be written through it.
    ///
    /// `Some(true)` written, `Some(false)` refused (wrong size, or the
    /// copy failed), `None` no device buffer for this handle yet -- in
    /// which case the caller's host-side store is the only copy and
    /// updating it is sufficient. Default impl answers `None` (no GPU
    /// offload); the VM override calls
    /// `runtime::offload::device_cache::upload_from_bytes`.
    fn gpu_array_upload_bytes(&self, _handle: u64, _bytes: &[u8]) -> Option<bool> {
        None
    }

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
    /// `obj_cid=0` — see `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md`,
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

    /// Is `addr` one of the VM's `Unsafe`-arena handles rather than a real,
    /// dereferenceable OS pointer?
    ///
    /// This is the distinction [`Self::copy_from_native_memory`] already makes
    /// internally, surfaced so a caller can *report* which population it is
    /// serving without attempting a transfer. `cratonvm-native-io`'s socket
    /// census uses it to answer a question that decides whether a whole class
    /// of optimisation is reachable at all: the bounce buffer in the direct
    /// `ByteBuffer` transfer path is removable only for a REAL pointer, since
    /// an arena handle cannot be handed to the kernel. Measuring the split on a
    /// live HTTP workload is what tells you whether that work is worth doing —
    /// and the answer must come from a counter, not from an assumption about
    /// which allocator the application used.
    ///
    /// It exists on this trait rather than as a direct call into
    /// `cratonvm-native-builtins` (which owns the tag bit) because
    /// `native-builtins` DEPENDS on `native-io`; the reverse edge would be a
    /// dependency cycle. Duplicating the tag constant into `native-io` was the
    /// other option and is worse — a second copy of a magic number that must
    /// track the arena allocator's, with nothing to notice when it stops.
    ///
    /// The default answers `false` — "assume a real pointer". A context that
    /// does not model the arena has no handles to misreport, and the only
    /// consumer is diagnostic.
    fn native_addr_is_arena_handle(&self, _addr: i64) -> bool {
        false
    }

    /// [`Self::class_id_of_object`], but resolved through the read barrier the
    /// way [`Self::get_field_by_name`] resolves it.
    ///
    /// The two are NOT interchangeable for a caller that memoizes field slots.
    /// `class_id_of_object` validates the address and answers `ClassId(0)` when
    /// it is not a live object base — but if a moving collection relocated the
    /// object and something else has since been allocated at the old address,
    /// it answers the class of THAT object instead. Pairing such an id with
    /// [`Self::get_field`], which forwards internally, would read the new
    /// object's slot layout out of the forwarded original: a wrong field, in
    /// bounds, with nothing raised.
    ///
    /// `get_field_by_name` never has that problem because it forwards first and
    /// derives the id from the forwarded object. Any caller resolving an index
    /// once and reusing it must do the same, which is what this exists for.
    ///
    /// The default delegates, because a context with no moving collector has
    /// nothing to forward.
    fn class_id_of_object_forwarded(&self, obj: ObjectRef) -> ClassId {
        self.class_id_of_object(obj)
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
    /// # This spelling cannot report a refusal — prefer the fallible one
    ///
    /// [`Self::try_ensure_synthetic_class`] is the same operation with an
    /// error channel, and it is what a native should call whenever its own
    /// signature can carry a failure (`MethodCallResult` and friends —
    /// `ClassIdentityError` converts into `MethodCallFailed` with `?`).
    ///
    /// "Never fails" above is a statement about the *signature*, not about
    /// reality. There is one question this operation genuinely cannot answer:
    /// a `name` that two or more **distinct** classes already carry. Minting a
    /// stand-in for it is the defect this pair exists to remove — the stand-in
    /// is filed under the bootstrap loader, which is probed first, so it then
    /// outranks every real class that made the name ambiguous. Behaviour when
    /// that happens:
    ///
    /// * **this method** does *not* mint under `name`. The VM's context hands
    ///   back a distinctly-named, correctly-sized
    ///   `cratonvm/synthetic/AmbiguousName$…` stand-in: safe to allocate
    ///   against, and it fails every identity question about `name` (a
    ///   `checkcast` to `name` is false, a method lookup finds nothing) so the
    ///   failure surfaces at the call that depended on the answer, naming the
    ///   requested class.
    /// * **the default implementation** below returns `ClassId::new(0)`, as it
    ///   always has.
    ///
    /// Neither is a guess between the ambiguous candidates, and neither
    /// registers anything under `name`.
    ///
    /// The default implementation falls back to `ClassId::new(0)` so mocks
    /// and non-VM contexts still compile; real VM contexts override it.
    ///
    /// # Provenance
    ///
    /// `#[track_caller]`, and it must stay that way. `ClassManager::
    /// admit_compatibility_class` records the Rust `Location::caller()` as the
    /// census's `requested_by`, and without this attribute every native in the
    /// workspace is reported as the one line of `NativeContextImpl` that
    /// forwards the call — which is what the 2026-08-05 census found (all
    /// seven native-minted classes attributed to `vm_exec.rs:13670`). The
    /// attribute is free at runtime for callers that never fabricate.
    ///
    /// # The infallible spelling is GONE
    ///
    /// `ensure_synthetic_class` — same operation, no error channel — was
    /// deleted on 2026-08-10 by JDK-only wave 2 step 3, along with
    /// `ClassManager::ensure_synthetic_class` behind it. It recorded the
    /// `--jdk-only` violation and then fabricated anyway, so a strict run
    /// reported a violation while continuing in the exact state contract §5
    /// forbids, and no signature in the workspace could say otherwise. Every
    /// caller now either propagates the refusal, absorbs it at a site that
    /// documents why, or — if what it wants is a VM-generated shape rather than
    /// a compatibility stand-in — asks [`Self::ensure_vm_internal_class`],
    /// which is a different question with a different answer.
    ///
    /// This is the fallible spelling: the same operation, with a channel for
    /// the two answers that are not a `ClassId`.
    ///
    /// # The ambiguity contract
    ///
    /// [`ClassIdentityError::AmbiguousName`] means the name is carried by two
    /// or more distinct classes and the VM refuses to pick or to mint a third.
    /// **A native that receives it should refuse in turn** — propagate with
    /// `?`, or re-ask with an initiating loader
    /// ([`NativeClassAccess::class_id_by_name_and_loader`],
    /// [`NativeClassAccess::class_id_by_name_via_referencing_class`]) if it has
    /// one. It must not fall back to a fabrication, to
    /// `ClassId::new(0)`, or to any same-named class of its own choosing:
    /// every one of those is the guess the refusal exists to prevent, and two
    /// distinct classes treated as one is type confusion — it defeats the
    /// verifier and produces machine code that reads the wrong object layout.
    ///
    /// **The stated consequence:** a shim that hits this stops working, and
    /// its Java-visible failure is an exception from the refusing native
    /// rather than a wrong answer. That is the intended trade. It is reachable
    /// only when an application really does have two distinct classes under
    /// one name (two `GroovyClassLoader$InnerLoader`s compiling the same
    /// script name, a webapp loader shadowing a container class), which is
    /// exactly the situation in which the old behaviour silently corrupted
    /// both.
    ///
    /// [`ClassIdentityError::Refused`] is the `--jdk-only` policy refusal:
    /// propagate it, retrying cannot help.
    ///
    /// # Default implementation
    ///
    /// `Ok(ClassId::new(0))` — the answer the deleted infallible default always
    /// gave, so mocks and non-VM contexts are unaffected.
    ///
    /// `#[track_caller]` for the reason stated above.
    #[track_caller]
    fn try_ensure_synthetic_class(
        &mut self,
        name: &str,
        num_fields: usize,
    ) -> Result<ClassId, ClassIdentityError> {
        let _ = (name, num_fields);
        Ok(ClassId::new(0))
    }

    /// Register (or look up) a **VM-generated** class — the other half of the
    /// §5 API boundary, and the reason deleting the infallible compatibility
    /// spelling did not have to break dynamic proxies.
    ///
    /// [`Self::try_ensure_synthetic_class`] mints *compatibility stand-ins*,
    /// the one thing `--jdk-only` forbids. This mints the classes a conforming
    /// JVM creates without any class file — array-adjacent shapes, lambda and
    /// proxy implementation classes and their superclasses, reflection
    /// accessors, and the VM's own internal allocation shapes. Contract §1 item
    /// 6 permits those in every mode, so **this never refuses and never records
    /// a violation**, and it is infallible for that reason rather than by
    /// oversight.
    ///
    /// **Do not reach for it to silence a refusal.** Contract §11's
    /// zero-stub census becomes unfalsifiable if a compatibility stand-in is
    /// minted through this door: the substitution continues and the report goes
    /// green. `ClassManager::ensure_generated_class` behind it `debug_assert`s
    /// on a `CompatibilityStub` origin, which a release build will not catch —
    /// the assertion is a backstop, not the decision. The decision is whether
    /// the JVM specification says a class file must exist for this name.
    ///
    /// # Default implementation
    ///
    /// `ClassId::new(0)`, matching the fallible sibling's default, so mocks and
    /// non-VM contexts compile unchanged. Real VM contexts override it.
    #[track_caller]
    fn ensure_vm_internal_class(&mut self, name: &str, num_fields: usize) -> ClassId {
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

    /// Start the recorder used by the real-JDK jdk.jfr.Event boundary.
    /// The default keeps lightweight NativeContext mocks independent of JFR.
    fn jfr_begin_java_recording(&mut self) {}

    /// Stop the recorder used by the real-JDK jdk.jfr.Event boundary.
    fn jfr_end_java_recording(&mut self) {}

    /// Whether the real-JDK JFR boundary currently has a running recording.
    fn jfr_java_recording_active(&self) -> bool {
        false
    }

    /// Record one Java `jdk.jfr.Event.commit()` through the VM recorder.
    ///
    /// `event_name` is the **JFR event name** — the `@Name` value when the
    /// event class carries one, else its binary class name — not the internal
    /// class name. That is what a consumer sees from
    /// `RecordedEvent.getEventType().getName()` and what
    /// `RecordingStream.onEvent(String, …)` matches on, so the name has to be
    /// canonicalised on the way in rather than decorated here.
    ///
    /// `fields` carries `(field name, JVM field descriptor, current value)` in
    /// the order the event type declares them. The descriptor is what lets the
    /// VM register the right JFR field type — `Value::Int` alone cannot
    /// distinguish a `boolean` from an `int` — and a `String` field arrives as
    /// its `Value::Object` because only the VM side can read the characters
    /// out of the heap.
    fn jfr_emit_java_event(
        &mut self,
        _event_name: &str,
        _fields: &[(String, String, Value)],
        _start_ns: u64,
        _duration_ns: u64,
    ) {
    }

    /// Apply the settings a `jdk.jfr.Recording` carries to the VM recording the
    /// Java boundary is driving.
    ///
    /// `enabled_names` is the set of JFR event names the Java side enabled.
    /// `None` means "no name filter" — record everything, which is what
    /// CratonVM's own recordings want. `Some(&[])` means **record nothing**, and
    /// that distinction is the point: a `new Recording()` with no `enable(...)`
    /// call records no events on HotSpot, so an empty list cannot be allowed to
    /// mean "everything".
    ///
    /// `thresholds` is `(event name, minimum duration in nanoseconds)`; an event
    /// shorter than its threshold is dropped.
    ///
    /// Both are keyed by NAME because the Java side knows which events are
    /// enabled before any of them has been committed, and a CratonVM event type
    /// gets its id at first commit.
    fn jfr_configure_java_recording(
        &mut self,
        _enabled_names: Option<&[String]>,
        _thresholds: &[(String, u64)],
    ) {
    }

    /// Remember the output requested by the JDK recorder so stopping it can
    /// flush the VM recording to the same path.
    fn jfr_set_java_output(&mut self, _path: &str) {}

    /// Dump the Java-owned recording to a caller-selected path.
    fn jfr_dump_java_recording(&mut self, _path: &str) {}

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

    /// Release a library previously returned by
    /// [`load_native_library`](Self::load_native_library) — the backing for
    /// `jdk.internal.loader.RawNativeLibraries.unload0`, whose real body
    /// `dlclose`s / `FreeLibrary`s the handle.
    ///
    /// Returns `true` if `lib_index` named a live library that this call
    /// retired, `false` for an out-of-range index, an already-unloaded one, or
    /// an implementation that cannot unload.
    ///
    /// # Contract
    ///
    /// This is a *logical* unload: implementations MUST make subsequent
    /// [`find_native_symbol`](Self::find_native_symbol) calls for the index
    /// fail, but are NOT required to unmap the library. Keeping the mapping
    /// resident is the safe direction of the two errors — a caller can still
    /// hold function pointers obtained from an earlier lookup (a bound FFM
    /// downcall stub does), and unmapping underneath those segfaults, while a
    /// retained mapping costs only address space. Indices are never recycled
    /// while a load is live, so an unloaded index stays unloaded until
    /// `load_native_library` hands that index out again.
    ///
    /// The default implementation is a no-op returning `false`, preserving the
    /// behaviour of implementations that have no unload path.
    fn unload_native_library(&mut self, _lib_index: i64) -> bool {
        false
    }

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
pub trait NativeContext: NativeSystemAccess + NativeExceptionAccess + NativeGpuAccess {}

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
    /// `stackwalk-and-vtable.md`). This exists so
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

/// Event emitted by the native `ByteArrayInputStream` implementation when a
/// reader reaches the end of the buffer, or closes it.
///
/// The mirror image of [`BaosEvent`], and it exists for the mirror-image
/// reason. A bridge API can hand Java a `ByteArrayInputStream`-shaped object
/// whose *lifetime* matters to native state the bridge holds elsewhere — the
/// `https:` response body is the motivating case: HotSpot returns the
/// connection to its `KeepAliveCache` the moment the body is drained, after
/// which every CONNECTION-level accessor throws `IllegalStateException:
/// connection not yet open` again, while the `SSLSession` object the
/// application already holds stays valid. CratonVM reads that body to
/// completion inside `perform` and hands it over whole, so the drain is the
/// only observable "the application is done with this exchange" instant, and
/// it is observable *here* and nowhere else.
///
/// Keeping the observation at the API boundary is what avoids making
/// `native-io` depend on a higher-level protocol crate, exactly as for
/// [`BaosEvent`].
///
/// # This adds no registration
///
/// `ByteArrayInputStream.read()I`, `read([BII)I` and `close()V` are **already**
/// registered natives owned by `native-io` (MEASURED — `--dump-native-registry`
/// under `--jdk-only`, `9ae371468`: `owns_slot=true` on all three, at
/// `native-io/src/lib.rs`). A hook inside an existing body registers nothing,
/// so `regression-suite/bridge-ratchet.sh` and the baselines under `scripts/`
/// do not move. Every design that instead added a `Bridge` over concrete
/// bytecode — a dedicated response-stream class, a `SequenceInputStream`
/// sentinel, a second `close()V` registration — would have needed a baseline
/// refresh; see
/// `docs/known-issues/jdk-only/G48-1-the-input-side-hook-and-a-gate-that-could-go-quiet-20260817.md`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BaisEvent {
    /// A read returned `-1`: `pos >= count`, the buffer is exhausted.
    ///
    /// **Fires on EVERY exhausted read, not only on the transition**, because
    /// the transition is not observable: a stream constructed empty is at
    /// `pos >= count` from its first read, and the two states are
    /// indistinguishable from inside the read body. Observers must therefore be
    /// idempotent — which they must be anyway, since a drained stream that is
    /// then closed produces `Eof` *and* [`BaisEvent::Close`].
    Eof,
    /// `close()` was called on the stream.
    Close,
}

/// Observer for [`BaisEvent`].
///
/// # Why this returns `()` and [`BaosEventHook`] returns `bool`
///
/// **Deliberate, and not an oversight in the mirroring.** A `BaosEvent`
/// observer legitimately *takes over* the write — the bytes go to a native sink
/// instead of the in-heap buffer — so `Ok(true)` meaning "consumed, skip the
/// ordinary path" is a real choice with a real second branch behind it.
///
/// There is no such choice on the input side. `ByteArrayInputStream.read()`
/// **must** return `-1` at `pos >= count` and `close()` **must** be a no-op, on
/// every path, whatever any observer thinks; those are JDK contracts a lifetime
/// observer has no business editing. A `bool` here would be a return value that
/// every caller is required to ignore — the kind of parameter that eventually
/// gets honoured by someone who reads the type and not the doc, silently
/// turning an EOF into a non-EOF. So the type says what is true: observe, do
/// not decide.
///
/// `Err` is still available and still propagates, for a genuine internal
/// failure the VM must not swallow. Observers should treat it as such and
/// **must not** raise a Java exception from here to signal an ordinary
/// condition: it would surface out of a `ByteArrayInputStream.read()` that
/// HotSpot completes normally, which is a divergence bought in exchange for
/// nothing.
pub type BaisEventHook =
    fn(&mut dyn NativeContext, ObjectRef, BaisEvent) -> Result<(), MethodCallFailed>;

static BAIS_EVENT_HOOK: OnceLock<BaisEventHook> = OnceLock::new();

/// Install the process-wide optional BAIS lifetime observer. Registration
/// happens during native bootstrap; repeated registrations are harmless because
/// the first (and only) bridge implementation wins — same contract as
/// [`install_baos_event_hook`].
pub fn install_bais_event_hook(hook: BaisEventHook) {
    let _ = BAIS_EVENT_HOOK.set(hook);
}

/// Offer a BAIS lifetime event to the optional observer.
///
/// With no hook installed this is one relaxed `OnceLock` load and a return,
/// which is strictly cheaper than what [`dispatch_baos_event`] already pays on
/// every single byte written through `ByteArrayOutputStream.write(I)V`.
pub fn dispatch_bais_event(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    event: BaisEvent,
) -> Result<(), MethodCallFailed> {
    match BAIS_EVENT_HOOK.get() {
        Some(hook) => hook(ctx, stream, event),
        None => Ok(()),
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
/// The registry has no default kind: `current_category` is `None` outside a
/// `set_category` / `with_category` scope, and a registration made there falls
/// back to `SyntheticStub` — the conservative choice, so anything an author
/// forgets to tag stays visible to the audit and gateable, never silently
/// trusted. `NativeMethodRegistry::effective_category` is the only place that
/// fallback is applied, and every registration that takes it is counted:
/// `category_chosen_log`, the census's `kind_chosen`, and the
/// `no_registration_runs_on_the_ambient_default` gate in
/// `native-builtins/tests/stub_ratchet.rs`.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum NativeKind {
    Intrinsic,
    Bridge,
    SyntheticStub,
}

/// Per-kind dispatch total, carrying the one fact that says whether it may be
/// believed. Produced by
/// [`NativeMethodRegistry::invocations_of_kind_checked`].
///
/// The pair exists because the total alone is unfalsifiable in the direction
/// gates actually assert. `synthetic_stub_invocations == 0` is the L4 gate
/// (`docs/feature-designs/jdk-only-mode.md` §4); it is read from a sum of
/// [`NativeMethodRegistry::record_invocation`] counters, and any slot wired to a
/// bypassing dispatch path contributes nothing to that sum however many times
/// it runs. Four such slots are `SyntheticStub` today — the Panama
/// `DowncallHandle` arms in `UNCOUNTED_STACKLESS_NATIVES`; see
/// [`NativeMethodRegistry::invocations_of_kind`] for the measurement. Reading
/// `total` without `incomplete_slots` cannot distinguish those two zeroes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct KindInvocations {
    /// The kind this was measured for.
    pub kind: NativeKind,
    /// Counted dispatches summed over every slot of this kind — a **floor** on
    /// Java-level calls whenever `incomplete_slots > 0`.
    pub total: u64,
    /// Slots of this kind that have declared their count a floor
    /// ([`NativeMethodRegistry::mark_invocations_incomplete`]).
    pub incomplete_slots: usize,
}

impl KindInvocations {
    /// `true` when `total == 0` **and** every slot of this kind is counted, so
    /// the zero actually means "none ran".
    ///
    /// A zero `total` with `incomplete_slots > 0` is not a weaker result than
    /// this — it is *no result*, and it is the state in which an `== 0`
    /// assertion passes while proving nothing.
    pub fn is_conclusive_zero(self) -> bool {
        self.total == 0 && self.incomplete_slots == 0
    }

    /// Whether the instrument can currently see every slot of this kind, i.e.
    /// whether `total` is a total rather than a floor.
    ///
    /// Distinguished from [`Self::is_conclusive_zero`] so a caller that wants
    /// to *report* a non-zero total can still say whether it is the whole
    /// number.
    pub fn is_measurable(self) -> bool {
        self.incomplete_slots == 0
    }

    /// The gate predicate: no dispatches of this kind were counted **and** none
    /// could have escaped counting.
    ///
    /// Identical to [`Self::is_conclusive_zero`] and named for the call site,
    /// because the two readings are worth keeping apart in a gate's source:
    /// `is_conclusive_zero` is a statement about the measurement,
    /// `is_clean` is the verdict drawn from it. A gate must not pass on
    /// `total == 0` alone.
    pub fn is_clean(self) -> bool {
        self.is_conclusive_zero()
    }

    /// One line a failing gate can print that names *which* of the two failure
    /// modes it hit — a gate that says only "assertion failed" sends the reader
    /// to look for a stub that may not exist.
    pub fn describe(self) -> String {
        match (self.total, self.incomplete_slots) {
            (0, 0) => format!("{}: 0 dispatches, all slots counted", self.kind.as_str()),
            (0, n) => format!(
                "{}: 0 counted dispatches, but {n} slot(s) of this kind are declared \
                 incomplete — this zero proves nothing (see NativeMethodRegistry::\
                 invocations_of_kind)",
                self.kind.as_str()
            ),
            (t, 0) => format!("{}: {t} dispatches", self.kind.as_str()),
            (t, n) => format!(
                "{}: at least {t} dispatches ({n} slot(s) declared incomplete, so this \
                 is a floor)",
                self.kind.as_str()
            ),
        }
    }
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

    /// Whether this kind may be registered/invoked under `mode`
    /// (`docs/feature-designs/jdk-only-mode.md` §1.3, §4, 2026-07-31).
    ///
    /// `SyntheticStub` is the ONLY kind `JdkOnly` rejects. `Bridge` and
    /// `Intrinsic` are both explicitly allowed by §1 ("native code may cross VM
    /// boundaries or provide proven intrinsics"), so this is deliberately not
    /// "everything except a reviewed allowlist" — the review is a human process
    /// recorded in the census, not a predicate here.
    ///
    /// Kept `#[inline]` and total: dispatch calls it per native invocation, and
    /// under `Compatible` it must collapse to a constant `true`.
    #[inline]
    pub fn allowed_in(self, mode: CompatibilityMode) -> bool {
        match mode {
            CompatibilityMode::Compatible => true,
            CompatibilityMode::JdkOnly => !matches!(self, NativeKind::SyntheticStub),
        }
    }
}

/// One row of the schema-version-2 native census
/// (`docs/feature-designs/jdk-only-mode.md` §4).
///
/// Schema v1 was `dump_registrations()`'s `(class, method, descriptor, kind)`
/// tuple, which could not answer either of the two questions the JDK-only gate
/// actually asks: *who* registered a stub (so it can be deleted at the source)
/// and *whether anything called it* (so a stub nobody dispatches is not treated
/// as a live blocker). Both were previously unrecoverable after boot — the
/// registration log kept only the triple, and re-registration of a triple
/// last-write-wins into the slot with no history at all.
///
/// One row per *registration*, not per slot: a triple registered twice appears
/// twice, in registration order, and only the surviving row carries the
/// invocation count (see [`NativeMethodRegistry::census`]).
#[derive(Debug, Clone)]
pub struct NativeCensusEntry {
    pub class: String,
    pub name: String,
    pub descriptor: String,
    pub kind: NativeKind,
    /// Registration site, when recorded (`"native-builtins/src/lib.rs:1234"`).
    /// `None` only for rows registered before provenance tracking existed —
    /// currently unreachable, but the contract keeps the `Option`.
    pub registered_by: Option<String>,
    /// Kind of the entry this registration overwrote, if any. `Some` means this
    /// row superseded an earlier registration of the identical triple.
    pub overwrote: Option<NativeKind>,
    /// Times this slot was dispatched **through a path that resolved it by
    /// name or id** this run. `0` on a superseded row: the count belongs to
    /// whoever currently owns the slot.
    ///
    /// # This is a lower bound, and reading it as a call count has cost time
    ///
    /// Not "times the method ran". A dispatch served from a pre-resolved
    /// function pointer — the interpreter's intrinsic table, the JIT's thin
    /// direct-call helpers — never touches the registry and is not here.
    /// MEASURED 2026-08-17: 100,000 `Math.abs` calls report `1`; the same run
    /// under `CRATONVM_DISABLE_INTRINSICS=1` reports `100,000`. 100,000
    /// `HashMap.get` calls report `1,873` with the JIT on and `100,001` under
    /// `--nojit`. Full method, controls and the causal test:
    /// `docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`.
    ///
    /// Consequences already paid for in this directory: a lane concluded a
    /// body was dead from `invocations = 0` when its workload simply never
    /// reached the counted path, and another was told to read the field as a
    /// boolean. **Check [`Self::invocations_complete`] first.** For a census
    /// that is exact for everything measured, run with `--nojit` and
    /// `CRATONVM_DISABLE_INTRINSICS=1`.
    pub invocations: u64,
    /// Whether [`Self::invocations`] is a total (`true`) or a floor (`false`)
    /// for this slot — see
    /// [`NativeMethodRegistry::mark_invocations_incomplete`].
    ///
    /// `true` is the default and means "no dispatch path has declared itself a
    /// bypass for this slot". Until the sites listed on that method actually
    /// call it, `true` is the answer for every row, and the honest reading of
    /// the whole column is still the one on [`Self::invocations`].
    pub invocations_complete: bool,
    /// Whether this registration still **owns its slot**, i.e. whether a
    /// dispatch of this triple would reach *this* row's callback.
    ///
    /// `false` means a later `register*` of the identical triple replaced it
    /// (last-write-wins into the slot), so the row records that a registration
    /// happened and nothing more: it can never be dispatched, and adjudicating
    /// its kind decides nothing.
    ///
    /// # Why this is a column and not left to the reader
    ///
    /// Every consumer of this census was inferring it — or not inferring it.
    /// Measured on `JdkOnlyCensusLoadProbe` (JDK 25, 2026-08-06): of 11,876
    /// rows, **1,237 own no slot**, and **1,092 of the 9,660 rows the
    /// adjudication reports as unadjudicated `Bridge` are among them**. So the
    /// live unadjudicated surface is 8,568, and a reclassification wave sized
    /// off the larger number would go looking for ~1,100 registrations that
    /// cannot affect anything.
    ///
    /// It is derivable from census order (within a triple, the last row owns the
    /// slot) — which is exactly why it belongs here instead: that is an
    /// undocumented ordering guarantee for external scripts to depend on, and
    /// [`NativeMethodRegistry::census`] already has the answer in hand.
    ///
    /// This is the fourth distinct way this census has been misread; the other
    /// three are in
    /// `jdk-only-census-one-class-one-platform-FIXED-20260810.md`.
    pub owns_slot: bool,
    /// Whether [`Self::kind`] was **stated at this registration site**
    /// (`register_with_kind`) or inherited from an ambient `set_category` in
    /// some enclosing registrar (`register`).
    ///
    /// This is the column the 157-entry reclassification was blocked on.
    /// `registered_by` says *where* a registration was written; only this says
    /// whether anybody decided what it is. A `false` here on a `SyntheticStub`
    /// row means nothing more than "no `set_category` covered this call site",
    /// since `SyntheticStub` is the default — which is very different from a
    /// deliberate stub, and the two were previously indistinguishable.
    ///
    /// Read it with the direction of the mistake in mind: `false` on a
    /// `Bridge` row is the *dangerous* one, because `Bridge` is never the
    /// default and can only have been inherited from a `set_category` line that
    /// covered more registrations than its author was thinking about.
    pub kind_stated: bool,
    /// Whether ANYONE chose [`Self::kind`] — at the registration site
    /// (`register_with_kind`) **or** in an enclosing `set_category` /
    /// `with_category` scope — as opposed to the registration simply landing on
    /// the constructor's `SyntheticStub` default.
    ///
    /// Deliberately wider than [`Self::kind_stated`], and it answers a
    /// different question. `kind_stated` asks *"was this adjudicated here?"*;
    /// this asks *"is this kind an opinion at all?"*. `with_category`'s own doc
    /// calls itself "how a whole `register_*` function tags all of its
    /// registrations" — that is an opinion expressed once for a group, and a
    /// row covered by one is not on the default.
    ///
    /// This is the column step 3 of
    /// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up
    /// is scored against: *"flip the default last — once every registration
    /// states its kind, `current_category` can default to something that fails
    /// loudly (or be deleted)"*. `kind_stated` cannot score it, because it is
    /// `false` on the thousands of rows a `with_category` scope covers
    /// deliberately. `kind_chosen: false` is the honest count of registrations
    /// nobody has an opinion about, and the default cannot be made to fail
    /// until it is zero.
    ///
    /// `false` does **not** mean the kind is `SyntheticStub`: `register`'s
    /// downgrade rule preserves a prior *chosen* kind over an unchosen
    /// re-registration of the same triple, so an unchosen row can carry any
    /// kind the earlier registration chose.
    pub kind_chosen: bool,
}

// ===========================================================================
// DUPLICATE-REGISTRATION ANALYSIS  (the shadowed-native gate)
// ===========================================================================

/// One registration that a LATER `register*` of the identical triple displaced.
///
/// # The defect species this exists to make findable
///
/// [`NativeMethodRegistry::register`] is last-write-wins and **updates the
/// existing slot in place**. So a correct, guarded, carefully-written native
/// silently loses to an unguarded twin registered later, and every symptom then
/// points at the wrong source file. Four instances cost a full
/// measure-fix-rebuild cycle each before anyone looked for the pattern:
///
/// 1. `MethodHandles$Lookup.defineHiddenClass` — a placeholder registered late
///    in `lang_invoke.rs` shadowed the real implementation, and returned a
///    `Lookup` whose slot 0 was never written, so `lookupClass()` answered null.
/// 2. `Files.copy(Path,Path,CopyOption...)` — the winner never read its options
///    argument, so a copy onto an existing file overwrote instead of throwing
///    `FileAlreadyExistsException`.
/// 3. `Module.getResourceAsStream` — registered twice from inside the SAME
///    function, ~9k lines apart; the wave-2 `opens` gate was dead code.
/// 4. `SSLContext.getInstance` — four competing registrations; the live one
///    threw an `IOException` whose message said `NoSuchAlgorithmException`.
///
/// The misdirection that made these expensive is worth stating once: **forcing
/// "the native" over real bytecode does not help when the slot holds a
/// DIFFERENT native.** Only provenance distinguishes the two, and only the
/// census keeps it.
///
/// # Why this is mechanical
///
/// Nothing here is inferred. `register` is `#[track_caller]`, so every accepted
/// registration already records its `Location`, and `registrations` is
/// append-only — the LOSER's row survives, tagged
/// [`NativeCensusEntry::owns_slot`]`== false`. The pair is therefore recoverable
/// exactly, with no heuristics and no source scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedRegistration {
    /// Class of the shadowed triple.
    pub class: String,
    /// Method name of the shadowed triple.
    pub name: String,
    /// Descriptor of the shadowed triple.
    pub descriptor: String,
    /// `file:line` of the registration that LOST, i.e. the callback that can
    /// never be dispatched. `None` only if provenance was not recorded.
    pub shadowed_at: Option<String>,
    /// [`NativeKind`] the losing registration carried.
    pub shadowed_kind: NativeKind,
    /// `file:line` of the registration that currently OWNS the slot — the
    /// callback a dispatch of this triple actually reaches.
    pub winner_at: Option<String>,
    /// [`NativeKind`] of the winning registration.
    pub winner_kind: NativeKind,
}

/// Strip the `:line` suffix off a `"file:line"` provenance string and normalise
/// `\` to `/`.
///
/// Both halves matter for a frozen baseline. `Location::file()` is the path as
/// the compiler saw it, which is backslash-separated on Windows and
/// forward-slash on Linux — a baseline keyed on the raw string is red on one of
/// the two hosts. And the LINE must go: a duplicate keyed on a line number goes
/// stale the moment anyone inserts a comment above the call, which is how fixed
/// line bands in this repo's source-witness tests have failed before.
fn provenance_file(site: Option<&str>) -> Option<String> {
    let site = site?;
    let file = match site.rfind(':') {
        Some(i) => &site[..i],
        None => site,
    };
    Some(file.replace('\\', "/"))
}

impl ShadowedRegistration {
    /// `class.name descriptor`, the way the rest of this crate spells a triple.
    pub fn triple(&self) -> String {
        format!("{}.{}{}", self.class, self.name, self.descriptor)
    }

    /// Source file of the losing registration, without its line number and with
    /// separators normalised. See [`provenance_file`].
    pub fn shadowed_file(&self) -> Option<String> {
        provenance_file(self.shadowed_at.as_deref())
    }

    /// Source file of the winning registration. See [`provenance_file`].
    pub fn winner_file(&self) -> Option<String> {
        provenance_file(self.winner_at.as_deref())
    }

    /// Whether the two registrations live in DIFFERENT source files.
    ///
    /// This is the shape of instances 1, 2 and 4 above, and the one a reviewer
    /// has no chance of catching by eye: the two files are never open at the
    /// same time. Instance 3 was same-file (and same *function*), which is why
    /// this is a classifier and not the gate's only question.
    pub fn cross_file(&self) -> bool {
        match (self.shadowed_file(), self.winner_file()) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }

    /// Whether the winner and the loser disagree about what this native *is*.
    ///
    /// The highest-signal subset. A `SyntheticStub` displacing a `Bridge` is
    /// instance 1 exactly: a placeholder overwriting a real implementation. It
    /// also changes behaviour beyond the callback, because `NativeKind` decides
    /// three separate things — `CompatibilityMode::JdkOnly` refuses a
    /// `SyntheticStub`, `CRATONVM_NO_STUBS` drops one, and
    /// `synthetic_stub_kind_should_yield_to_real_bytecode` arbitrates for one.
    pub fn kind_disagreement(&self) -> bool {
        self.shadowed_kind != self.winner_kind
    }

    /// Stable one-line key for a frozen baseline: the triple plus the two
    /// FILES, tab-separated, with no line numbers.
    ///
    /// Deliberately coarser than the full provenance. Moving a registration
    /// within its file must not turn the gate red, but moving it to a different
    /// file — or adding a third registrar — must, because that is a new pair a
    /// human has not looked at. Two different triples shadowed by the same file
    /// pair stay distinct rows, since the triple leads the key.
    pub fn key(&self) -> String {
        format!(
            "{}\t{} => {}",
            self.triple(),
            self.shadowed_file().unwrap_or_else(|| "<unknown>".into()),
            self.winner_file().unwrap_or_else(|| "<unknown>".into()),
        )
    }
}

/// Every row in `rows` that a later registration of the identical triple
/// displaced, paired with the registration that won.
///
/// Free function over a census rather than a method on the registry, because
/// the gate that consumes it lives in `native-builtins/tests` (it needs the
/// registrar crates, which `native-api` cannot depend on) while the logic must
/// be unit-testable here, where no boot path is available.
///
/// One row per LOSER, so a triple registered four times yields three rows, all
/// naming the same winner. That is the right shape for a ratchet: fixing one of
/// the four removes exactly one row.
///
/// The winner is the row with [`NativeCensusEntry::owns_slot`]`== true`, of
/// which a triple has exactly one — `register` either pushes a slot or rewrites
/// the existing slot's `reg_index` to point at the newest registration, so
/// `census`'s reverse map can only attribute the slot to the last accepted
/// registration. The `unwrap_or(last)` fallback below is unreachable and exists
/// so a hypothetical desync under-reports rather than panicking in CI.
pub fn shadowed_registrations_in(rows: &[NativeCensusEntry]) -> Vec<ShadowedRegistration> {
    // Group by triple, preserving first-seen order so the output is
    // deterministic across runs — the property any frozen baseline rests on.
    let mut order: Vec<(&str, &str, &str)> = Vec::new();
    let mut groups: FxHashMap<(&str, &str, &str), Vec<usize>> = FxHashMap::default();
    for (i, row) in rows.iter().enumerate() {
        let key = (
            row.class.as_str(),
            row.name.as_str(),
            row.descriptor.as_str(),
        );
        let bucket = groups.entry(key).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        bucket.push(i);
    }

    let mut out = Vec::new();
    for key in order {
        let idxs = match groups.get(&key) {
            Some(v) if v.len() > 1 => v,
            _ => continue,
        };
        let winner_idx = idxs
            .iter()
            .copied()
            .find(|i| rows[*i].owns_slot)
            .unwrap_or_else(|| idxs[idxs.len() - 1]);
        let winner = &rows[winner_idx];
        for &i in idxs {
            if i == winner_idx {
                continue;
            }
            let loser = &rows[i];
            out.push(ShadowedRegistration {
                class: loser.class.clone(),
                name: loser.name.clone(),
                descriptor: loser.descriptor.clone(),
                shadowed_at: loser.registered_by.clone(),
                shadowed_kind: loser.kind,
                winner_at: winner.registered_by.clone(),
                winner_kind: winner.kind,
            });
        }
    }
    out
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

/// The one package whose registrations can be retired at runtime — see
/// [`NativeMethodRegistry::mute_netty_tcnative_stubs`].
const NETTY_TCNATIVE_PACKAGE: &str = "io/netty/internal/tcnative/";

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
    /// Whether this callback satisfies the **leaf** contract — see
    /// [`NativeMethodRegistry::set_leaf`]. Dispatch paths that hold the
    /// `NativeMethodId` may then invoke it through `safe_native_call_leaf`
    /// instead of the full funnel.
    ///
    /// Carried on the slot rather than derived from the triple so that
    /// re-registration cannot leave a stale claim behind: the `Some(idx)` arm
    /// of [`register`](NativeMethodRegistry::register) overwrites this with
    /// whatever the *new* registration declared, and the default is `false`.
    /// A phase that re-registers a triple without opting in therefore
    /// demotes it back to the funnel, which is the safe direction.
    leaf: bool,
}

/// Where one accepted registration came from, index-parallel with
/// `registrations` / `categories`.
///
/// `&'static Location` is two words and is produced by the compiler from the
/// `#[track_caller]` shim on `register()` — no allocation, no `format!`, no
/// string at all until [`NativeMethodRegistry::census`] asks. That matters
/// because boot runs ~3,100 registrations and the census is requested by
/// roughly nobody: the same reasoning that moved the native-ring name map to
/// `name_index` + `flush_native_ring_names` (see that field doc).
struct RegistrationProvenance {
    /// `file()`/`line()` of the `register(...)` call. Formatted only in
    /// `census()`.
    site: &'static core::panic::Location<'static>,
    /// Kind of the slot this registration displaced, captured BEFORE the
    /// in-place slot update rewrites it. That history is otherwise
    /// unrecoverable — `register()` is documented last-registration-wins and
    /// the slot keeps no predecessor — and the JDK-only plan needs it to tell
    /// "this triple was always a bridge" from "a stub was promoted to a bridge
    /// after the fact" when auditing the census.
    overwrote: Option<NativeKind>,
}

pub struct NativeMethodRegistry {
    /// Dense slot table. `slots[i]` is the resolved native for
    /// `NativeMethodId(i)`. Append-only: entries are updated in place on
    /// re-registration and never removed, which is what makes handles stable.
    slots: Vec<NativeSlot>,
    /// Per-slot dispatch counter, **index-parallel with `slots`** and grown in
    /// the single `None` arm of `register()` that pushes a slot.
    ///
    /// Deliberately NOT a field on [`NativeSlot`]. `NativeSlot` is
    /// `#[derive(Copy, Clone)]` and is copied by value out of the table on the
    /// `find` / `find_with_kind` hot path (`slot_for_exact` hands back a `&`,
    /// both callers immediately destructure `slot.callback`/`slot.kind`); an
    /// `AtomicU64` is not `Copy`, so putting the counter there would force
    /// every dispatch lookup to carry the registry borrow's lifetime instead of
    /// two plain machine words. A parallel `Vec` keeps the counter off the
    /// dispatch struct entirely — the counter is touched only by
    /// [`record_invocation`](Self::record_invocation), which is a separate
    /// bounds-checked index.
    ///
    /// A superseded registration does not get its own counter: re-registering a
    /// triple updates the slot in place (that is what keeps handles valid), so
    /// the accumulated count carries over to the new owner. See
    /// [`invocations_of_kind`](Self::invocations_of_kind).
    slot_invocations: Vec<std::sync::atomic::AtomicU64>,
    /// Per-slot "this counter is a **lower bound**" flag, index-parallel with
    /// `slots` and grown in the same single arm as `slot_invocations`.
    ///
    /// # Why the census needs a second bit per slot
    ///
    /// [`record_invocation`](Self::record_invocation) is exact for every
    /// dispatch that reaches it. What it cannot see is a dispatch that never
    /// consults the registry at all, because some caller resolved this
    /// triple's callback ONCE and then called the resulting function pointer
    /// directly. Two such families were measured on 2026-08-17
    /// (`docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`):
    /// the interpreter's intrinsic table and the JIT's thin direct-call
    /// helpers. Both are deliberate optimisations, both are correct, and both
    /// silently removed this counter along with the name resolution it was
    /// attached to.
    ///
    /// Rather than pay a `fetch_add` on those paths — measured at **+9.2 ns
    /// per call**, which is the whole margin a thin direct-call helper exists
    /// to buy — a bypassing path sets this bit ONCE, cold, when it binds the
    /// call site. The census then reports `invocations_complete: false` for
    /// that slot, and a reader knows the number is a floor rather than a
    /// count. An instrument that says "at least N" is usable; one that says
    /// "N" and means "at least N" is not.
    ///
    /// `false` (the default) is the *claim*, not the absence of one: it says
    /// nothing bypassed this slot as far as the registry was told. It is only
    /// as true as the bypassing paths are honest about calling
    /// [`mark_invocations_incomplete`](Self::mark_invocations_incomplete) —
    /// which is why that method's doc carries the list of sites that must.
    slot_invocations_incomplete: Vec<std::sync::atomic::AtomicBool>,
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
    /// Set once this VM has loaded Netty's real `netty_tcnative` library and run
    /// its `JNI_OnLoad`; from then on every lookup under
    /// `io/netty/internal/tcnative/` misses, so dispatch falls through to the
    /// `RegisterNatives` pointers the library published.
    ///
    /// This lives on the registry rather than at a dispatch site because the
    /// registry is the only choke point all the routes share. `vm_exec`'s
    /// general `is_native` arm, `try_stackless_invoke`'s `resolve_step1_native`,
    /// and `dispatch_static`'s skip-clinit probe each reach the table
    /// independently, and a stub retired at one of them is still live at the
    /// others — which is not a cosmetic difference: it left
    /// `Library.initialize0()Z` (a plain zero-arg static, so the stackless path
    /// claims it) answering `true` without ever calling `apr_initialize`, while
    /// the rest of the package ran against the real library. The first real
    /// `SSLContext.make` then dereferenced the never-created `tcn_global_pool`
    /// and took a SIGSEGV.
    ///
    /// The registry is per-`SharedVm`, so muting is per-VM: a sibling VM that
    /// never loaded the library keeps its stubs.
    ///
    /// Only ever set, never cleared — a `dlclose` cannot un-publish the
    /// function pointers `find_jni_native` already holds.
    netty_tcnative_muted: std::sync::atomic::AtomicBool,
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
    /// Index-parallel with `categories` and `provenance`. Only *accepted*
    /// registrations are pushed — every drop arm in `register()` (including the
    /// JDK-only refusal) returns before this point.
    registrations: Vec<(Box<str>, Box<str>, Box<str>)>,
    /// Registration site + displaced kind, index-parallel with `registrations`
    /// and `categories`. See [`RegistrationProvenance`]; materialized into
    /// strings only by [`census`](Self::census).
    provenance: Vec<RegistrationProvenance>,
    /// AUDIT 2026-05-17 (Fix 5): O(1) index keyed by the 128-bit hash
    /// of `(method_name, descriptor)` (class portion omitted). Used by
    /// `find_by_method_descriptor` to avoid the O(N) linear scan over
    /// `registrations`. Built incrementally on every `register()`.
    by_method_desc: FxHashMap<(u64, u64), NativeCallback>,
    /// Memo for [`owner_classes_for_method`](Self::owner_classes_for_method):
    /// `(registrations.len() when built, (method, descriptor) -> owning classes)`.
    /// Interior-mutable because every caller holds the registry by `&self`.
    method_owner_index: std::sync::RwLock<
        Option<(
            usize,
            std::collections::HashMap<(Box<str>, Box<str>), Vec<Box<str>>>,
        )>,
    >,
    /// Category aligned with `registrations` (index-parallel), for
    /// `dump_registrations` / census output.
    categories: Vec<NativeKind>,
    /// The category applied to subsequent `register()` calls, or `None` when
    /// no scope is in effect.
    ///
    /// `None` is what makes "nobody chose this" a *scoped* fact rather than a
    /// sticky one. It used to be a plain `NativeKind` initialised to
    /// `SyntheticStub`, with a separate `category_chosen: bool` alongside — and
    /// that boolean was set by `set_category` and **never cleared**, because
    /// the restore half of the ubiquitous
    /// `let prev = registry.current_category(); … registry.set_category(prev);`
    /// idiom put the kind back and left the flag true. So after the first
    /// `set_category` anywhere in boot, every subsequent registration reported
    /// "chosen" whether or not any scope covered it: measured on JDK 25 /
    /// linux, 11,875 of 11,876 registrations, which is not a measurement of
    /// anything.
    ///
    /// Carrying the "chosen" bit *inside* the value fixes that for free. The
    /// idiom already saves and restores; an `Option` makes it restore the
    /// absence too, so a registrar entered with no category leaves none behind.
    /// `set_category` takes `impl Into<Option<NativeKind>>`, so both halves of
    /// the idiom — and all 567 `set_category(NativeKind::…)` sites — compile
    /// unchanged.
    ///
    /// Reads that need a kind rather than a question go through
    /// [`Self::effective_category`], which is the one place the conservative
    /// `SyntheticStub` default now lives.
    current_category: Option<NativeKind>,
    /// The leaf claim applied to subsequent `register()` calls. Scoped via
    /// [`Self::set_leaf`] / [`Self::with_leaf`]. Defaults to `false`
    /// (conservative — every native pays the full funnel until it opts out).
    current_leaf: bool,
    /// Whether `current_category` was **stated at the registration site**
    /// ([`NativeMethodRegistry::register_with_kind`]) rather than inherited
    /// from an ambient `set_category` / `with_category` in an ancestor frame.
    ///
    /// Index-parallel copy lands in `kind_stated`. Only ever `true` for the
    /// duration of one `register_with_kind` call, which sets and restores it
    /// around the inner `register`.
    ///
    /// This is the discriminator the 157-entry reclassification needs: today
    /// the census can say a registration is a `SyntheticStub` and where it was
    /// written, but not whether anyone *decided* that. See
    /// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.
    next_kind_stated: bool,
    /// Per-registration copy of [`Self::next_kind_stated`], index-parallel with
    /// `registrations` and `categories`.
    kind_stated: Vec<bool>,
    /// Per-registration answer to *"was a category scope in effect for this
    /// call?"* — i.e. `current_category.is_some()` at the moment of the
    /// `register`. Index-parallel with `registrations` and `categories`.
    ///
    /// Load-bearing for the downgrade rule in `register`. A first cut keyed
    /// that rule on `kind_stated` alone and promptly preserved a stated
    /// `Bridge` over a `with_category(Intrinsic)` re-registration of
    /// `java/lang/Float.intBitsToFloat` — a downgrade of exactly the kind the
    /// rule exists to prevent, in the opposite direction. `with_category` is an
    /// opinion; `kind_stated` deliberately does not record it, and this does.
    /// Caught by diffing `--dump-native-registry` across the change; it was the
    /// only kind that moved.
    category_chosen_log: Vec<bool>,
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
    /// Opt out of the `java/net/Socket` / `ServerSocket` drop below.
    ///
    /// That filter exists because ONE registered native shadows a class's real
    /// bytecode at every dispatch site, so under `CRATONVM_REAL_NET_SOCKETS`
    /// the synthetic socket surface has to be absent from the registry, not
    /// merely un-called. That is right for a running VM and wrong for a
    /// registry built purely to unit-test the synthetic natives themselves:
    /// there is no real socket bytecode in that process to shadow, so the
    /// filter just leaves the tests with nothing to call. Default `false`; only
    /// `cratonvm-vm`'s inline test registry sets it.
    allow_synthetic_net_sockets: bool,
    /// VM-scoped strict policy (`docs/feature-designs/jdk-only-mode.md` §4,
    /// 2026-07-31). Set once at VM init by
    /// [`set_compatibility_mode`](Self::set_compatibility_mode), before the
    /// `register_*` population pass.
    ///
    /// A **field**, not a process global, on purpose: §2 forbids process
    /// globals for this feature, and this repo has already shipped the failure
    /// it is protecting against — process-global native caches leaking state
    /// across two `SharedVm`s in one test process.
    ///
    /// Relationship to `drop_synthetic_stubs`: `JdkOnly` is a strict superset.
    /// `CRATONVM_NO_STUBS` silently drops the same bucket; `JdkOnly`
    /// additionally records a structured, attributed
    /// `JdkOnlyViolation::SyntheticNativeRegistered` in
    /// [`refused`](Self::refused_registrations). Both arms are live and
    /// independent — the env var keeps working with `Compatible` set.
    ///
    /// In `Compatible` (the default) the only cost anywhere is one field load
    /// and a discriminant compare at the top of `register()`; nothing in
    /// dispatch reads it.
    compatibility_mode: CompatibilityMode,
    /// Is a REAL JDK class library in use (`--java-home`) rather than the
    /// synthetic one?
    ///
    /// Distinct from [`CompatibilityMode`], which is the strict-vs-compatible
    /// policy — a run can be `Compatible` on either class library. Registrars
    /// consult this to skip a native whose only job is to stand in for a class
    /// the synthetic library fakes. See `native-collections`'
    /// `CompletableFuture` dependent-stage block: over a real JDK those
    /// natives detect the real object and delegate straight back to the method
    /// they shadow, so registering them buys a native dispatch and a by-name
    /// `invoke_special` and nothing else.
    ///
    /// Set **once at VM init, before the `register_*` population pass**, for
    /// the same reason [`Self::set_compatibility_mode`] must be: it gates
    /// registration, so flipping it later leaves whatever was already accepted
    /// in place. Defaults to `false`, so a registry nobody configures behaves
    /// exactly as it did before this field existed.
    real_jdk: bool,
    /// Registrations refused by the `JdkOnly` arm of `register()`, in
    /// registration order. Empty in `Compatible` mode. This is the wave-1
    /// deliverable: §10 is "measurement, not deletion" everywhere except class
    /// fabrication (§5) and stub registration (§4) — this vector is the §4 half
    /// of the evidence, and the CLI report renders it.
    refused: Vec<JdkOnlyViolation>,
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
    /// This VM's capability policy, or `None` when the embedder installed none.
    ///
    /// A **field**, not a process global, for exactly the reason
    /// [`compatibility_mode`](Self) is: a policy shared between two `SharedVm`s
    /// in one process is the cross-VM interference
    /// `crate::capability` exists to remove. `Arc` because the same policy
    /// object is also reachable from every native through
    /// [`CapabilityCheck`](crate::capability::CapabilityCheck), and the audit
    /// log must be one log, not two.
    ///
    /// `None` is the default and costs one null check in `register()` and one
    /// in [`check_dispatch_capability`](Self::check_dispatch_capability).
    capabilities: Option<Arc<crate::capability::CapabilitySet>>,
    /// `slot index -> capability kind` for the handful of registered natives
    /// that definitionally exercise a capability
    /// ([`classify_native`](crate::capability::classify_native)).
    ///
    /// Sparse on purpose: of ~3,100 registrations only a few dozen classify,
    /// so this is a small map rather than a byte on the `Copy` `NativeSlot`
    /// that the `find`/`find_with_kind` hot path copies by value. Populated in
    /// `register()` only when a policy is installed — with no policy there is
    /// nothing to check and the map stays empty.
    sensitive_slots: FxHashMap<u32, crate::capability::CapabilityKind>,
}

impl NativeMethodRegistry {
    pub fn new() -> Self {
        // ~3,100 native methods are registered at boot; size the maps
        // up front so `register()` does not repeatedly rehash/grow.
        const BOOT_REGISTRATION_HINT: usize = 4096;
        Self {
            slots: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            // Index-parallel with `slots`; sized identically so the ~3,100 boot
            // pushes never reallocate.
            slot_invocations: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            // Same index-parallel discipline and the same sizing hint; see the
            // field doc for what the bit means.
            slot_invocations_incomplete: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            slot_by_key: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            classes_with_natives: FxHashSet::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            registry_epoch: NEXT_REGISTRY_EPOCH
                .fetch_add(REGISTRY_EPOCH_STRIDE, std::sync::atomic::Ordering::Relaxed),
            netty_tcnative_muted: std::sync::atomic::AtomicBool::new(false),
            registrations: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            provenance: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            by_method_desc: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            method_owner_index: std::sync::RwLock::new(None),
            categories: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            current_category: None,
            current_leaf: false,
            next_kind_stated: false,
            kind_stated: Vec::new(),
            category_chosen_log: Vec::new(),
            // Read once at construction. `CRATONVM_NO_STUBS` (any non-empty
            // value) enables strict mode: synthetic-stub registrations are
            // dropped so calls hit real bytecode or a clear error.
            allow_synthetic_net_sockets: false,
            drop_synthetic_stubs: cratonvm_types::flags::runtime_var_os("CRATONVM_NO_STUBS")
                .is_some_and(|v| !v.is_empty()),
            // JDK-only policy is a CLI/init decision, never an env var: it must
            // be identical for every registry in a process-hosted multi-VM run
            // only if the caller says so. Default `Compatible` keeps today's
            // behaviour byte-for-byte (§1, "--real-jdk (default) keeps today's
            // behaviour exactly").
            compatibility_mode: CompatibilityMode::Compatible,
            real_jdk: false,
            refused: Vec::new(),
            drop_real_layout_synthetic: false,
            // PERF: deferred native-ring name index. Sized like the other boot
            // maps so the ~3,100 boot inserts don't rehash/grow.
            name_index: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            // No policy until an embedder installs one: today's behaviour
            // exactly. See `capabilities` field doc.
            capabilities: None,
            sensitive_slots: FxHashMap::default(),
        }
    }

    /// Install this VM's capability policy.
    ///
    /// Call **once at VM init, before the `register_*` population pass** — this
    /// gates `register()`, and it is what populates the dispatch-side
    /// classification map, so a policy installed afterwards leaves the already-
    /// registered natives unclassified. `&mut self` is the enforcement:
    /// dispatch only ever holds `&`, so a running native cannot swap the
    /// policy out from under the gate.
    ///
    /// Pass the same `Arc` to
    /// [`install_capabilities`](crate::capability::install_capabilities) so
    /// per-call-site gates reached through `NativeContext` share one audit log
    /// with the registry.
    pub fn set_capabilities(&mut self, caps: Arc<crate::capability::CapabilitySet>) {
        self.capabilities = Some(caps);
    }

    /// This VM's capability policy, if one was installed.
    #[inline]
    pub fn capabilities(&self) -> Option<&Arc<crate::capability::CapabilitySet>> {
        self.capabilities.as_ref()
    }

    /// The capability a registered native definitionally exercises, or `None`
    /// for the ~3,100 that exercise none (and for every registration accepted
    /// before a policy was installed).
    #[inline]
    pub fn capability_of_id(
        &self,
        id: NativeMethodId,
    ) -> Option<crate::capability::CapabilityKind> {
        self.sensitive_slots.get(&(id.index() as u32)).copied()
    }

    /// Dispatch-side capability gate: check the capability class of the native
    /// behind `id` before invoking it.
    ///
    /// This is the coarse safety net under the per-call-site gates. It can only
    /// report [`Scope::Any`](crate::capability::Scope::Any) — at this point the
    /// arguments have not been decoded, so there is no path or host to name —
    /// which means an `Enforce` deployment must hold the unscoped grant (e.g.
    /// `process-spawn:*`) for the class of native to dispatch at all, and the
    /// per-call-site gate then applies the scoped decision. Its value is
    /// coverage: it fires for every native in
    /// [`classify_native`](crate::capability::classify_native), including ones
    /// whose implementation has no gate of its own yet.
    ///
    /// With no policy installed this is one `Option` discriminant test.
    #[inline]
    #[track_caller]
    pub fn check_dispatch_capability(
        &self,
        id: NativeMethodId,
    ) -> Result<(), crate::capability::CapabilityDenied> {
        let Some(caps) = self.capabilities.as_ref() else {
            return Ok(());
        };
        let Some(kind) = self.capability_of_id(id) else {
            return Ok(());
        };
        caps.check(crate::capability::Capability::of(
            kind,
            crate::capability::Scope::Any,
        ))
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

    /// Set the VM-scoped strict policy
    /// (`docs/feature-designs/jdk-only-mode.md` §4). Call **once at VM init,
    /// before the `register_*` population pass** — this gates `register()`, so
    /// flipping it afterwards leaves whatever was already accepted in place and
    /// produces a registry that is neither honestly compatible nor honestly
    /// strict. `&mut self` is the enforcement: dispatch only ever holds `&`.
    ///
    /// Does not touch `drop_synthetic_stubs`; the `CRATONVM_NO_STUBS` env
    /// reading in `new()` is independent and still applies.
    pub fn set_compatibility_mode(&mut self, mode: CompatibilityMode) {
        self.compatibility_mode = mode;
    }

    /// The VM-scoped strict policy in force. One field load — callers on
    /// dispatch-adjacent paths may read it per call.
    #[inline]
    pub fn compatibility_mode(&self) -> CompatibilityMode {
        self.compatibility_mode
    }

    /// Declare that this registry is being populated for a REAL JDK class
    /// library. See [`Self::real_jdk`]; call once, before the `register_*`
    /// pass.
    pub fn set_real_jdk(&mut self, real_jdk: bool) {
        self.real_jdk = real_jdk;
    }

    /// Is a real JDK class library in use? Read by registrars deciding whether
    /// a synthetic-library stand-in is worth registering at all.
    #[inline]
    pub fn real_jdk(&self) -> bool {
        self.real_jdk
    }

    /// Registrations refused because of `JdkOnly`, in registration order.
    /// Always empty under `Compatible`.
    pub fn refused_registrations(&self) -> &[JdkOnlyViolation] {
        &self.refused
    }

    /// The refusals that did **not** retire their method — every
    /// [`JdkOnlyViolation::SyntheticNativeRegistered`] whose triple was already
    /// owned, so an earlier native survived and still serves in strict mode.
    ///
    /// Returns `(class, method, descriptor, survivor)`. This is the species
    /// `registrar_drift.rs` is structurally blind to (it compares a
    /// synthetic-only pass against a shipping one, and this is two SHIPPING
    /// passes) and that `duplicate_registration_gate.rs` names as its blind
    /// spot 3 (a dropped registration leaves no census row at all).
    pub fn refusals_that_left_a_survivor(&self) -> Vec<(&str, &str, &str, &str)> {
        self.refused
            .iter()
            .filter_map(|v| match v {
                JdkOnlyViolation::SyntheticNativeRegistered {
                    class,
                    method,
                    descriptor,
                    survivor: Some(survivor),
                    ..
                } => Some((
                    class.as_str(),
                    method.as_str(),
                    descriptor.as_str(),
                    survivor.as_str(),
                )),
                _ => None,
            })
            .collect()
    }

    /// `"<kind>@<file>:<line>"` of the registration that currently owns this
    /// triple, or `None` when nothing does.
    ///
    /// Called only from the `JdkOnly` refusal arm, which is bounded by the
    /// number of refusals rather than by the ~3,100 registrations, so the
    /// `format!` is affordable there for the same reason the site string is.
    fn surviving_owner(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<String> {
        let class_state = self.class_prefilter(class_name)?;
        let idx = self.slot_index_from_state(class_state, class_name, method_name, descriptor)?;
        let slot = self.slots.get(idx as usize)?;
        let site = self
            .provenance
            .get(slot.reg_index as usize)
            .map(|p| format!("{}:{}", p.site.file(), p.site.line()))
            .unwrap_or_else(|| "<no provenance>".to_string());
        Some(format!("{}@{}", slot.kind.as_str(), site))
    }

    /// Enable real-JDK-mode dropping of synthetic natives whose hardcoded
    /// field-slot layout corrupts the real JDK object (see
    /// [`drop_real_layout_synthetic`](Self)). Call before the `register_*`
    /// population pass in real-JDK mode.
    pub fn set_drop_real_layout_synthetic(&mut self, drop: bool) {
        self.drop_real_layout_synthetic = drop;
    }

    /// Whether this registry is being populated for a REAL-JDK arm.
    ///
    /// Read this at a registration SITE when a cluster has no working
    /// real-bytecode fallback to drop to and so cannot be expressed as a rule
    /// in `register` — the synthetic `FileInputStream`/`FileOutputStream`
    /// `<init>` block in `native-io` is the case this exists for. A
    /// `#[cfg(feature = "synthetic-jdk")]` guard is NOT equivalent and must
    /// not be used for this: the Cargo feature decides what is COMPILED, the
    /// launcher flag decides which CLASS LIBRARY loads, and a feature-enabled
    /// binary run `--real-jdk` satisfies the cfg while facing real JDK
    /// classes.
    pub fn drops_real_layout_synthetic(&self) -> bool {
        self.drop_real_layout_synthetic
    }

    /// Set the category applied to all subsequent `register()` calls until
    /// changed again. Prefer [`with_category`](Self::with_category) for a
    /// scoped set/restore.
    ///
    /// Takes `impl Into<Option<NativeKind>>` so both halves of the
    /// save/restore idiom this codebase uses in 564 places —
    /// `set_category(NativeKind::Bridge)` and `set_category(__prev_cat)` —
    /// compile unchanged while the restore now also restores the *absence* of
    /// a choice. See [`Self::current_category`].
    pub fn set_category(&mut self, kind: impl Into<Option<NativeKind>>) {
        self.current_category = kind.into();
    }

    /// The category currently applied to new registrations, or `None` if no
    /// scope covers them. Feed it straight back to [`Self::set_category`] to
    /// restore around a nested registrar.
    pub fn current_category(&self) -> Option<NativeKind> {
        self.current_category
    }

    /// The kind a registration made right now would carry: the scope's, or the
    /// conservative `SyntheticStub` when none covers it.
    ///
    /// The ONE place that default lives. Its doc comment's promise — "anything
    /// an author forgets to tag stays visible to the audit and gateable, never
    /// silently trusted" — is only kept if something can see the forgetting,
    /// which is what `category_chosen_log` and the census's `kind_chosen` are
    /// for.
    fn effective_category(&self) -> NativeKind {
        self.current_category.unwrap_or(NativeKind::SyntheticStub)
    }

    /// Run `f` with `current_category` set to `kind`, restoring the previous
    /// category afterwards. This is how a whole `register_*` function tags all
    /// of its registrations without touching individual `register()` calls.
    pub fn with_category(
        &mut self,
        kind: impl Into<Option<NativeKind>>,
        f: impl FnOnce(&mut Self),
    ) {
        let prev = self.current_category;
        self.current_category = kind.into();
        f(self);
        self.current_category = prev;
    }

    /// Declare that subsequent `register()` calls install **leaf** natives.
    ///
    /// # The contract a leaf callback must satisfy
    ///
    /// A leaf native may be invoked through `safe_native_call_leaf` — the
    /// funnel with its GC bookkeeping removed — so all four of these must hold
    /// for the registered body, on **every** path including its error returns:
    ///
    ///  1. **It cannot allocate on the Java heap.** No `alloc_object`,
    ///     `new_array`, `new_ref_array`, no string interning, no boxing.
    ///  2. **It cannot safepoint or block.** No `begin_blocking_region`, no
    ///     monitor acquisition, no lock on a structure another thread holds
    ///     across a safepoint, no `park`, no re-entry into Java.
    ///  3. **It cannot initiate or observe a collection.** No `maybe_gc_*`, and
    ///     no retained raw `ObjectRef` across anything that could move the heap
    ///     (which follows from 1 and 2).
    ///  4. **It cannot throw a JNI-pending exception.** Returning
    ///     `Err(MethodCallFailed)` is fine — the caller routes that exactly as
    ///     the funnel does — but nothing may go through
    ///     `jni::set_pending_exception`, because the leaf path does not drain
    ///     that slot.
    ///
    /// What is left is a field read, a field write, an atomic RMW, or pure
    /// arithmetic: the bodies for which the funnel's ~180-330 ns of pinning,
    /// STW probing, thread-state transitions and unwind bookkeeping is the
    /// entire cost of the call. Measured on `probes/NativeShapeProbe.java`;
    /// see `native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md`.
    ///
    /// The claim rides on the **callback**, not on the triple: it is captured
    /// into the slot by `register()` alongside the category, so a later phase
    /// that re-registers the same triple with a different body demotes it back
    /// to the full funnel unless that phase opts in as well. That ordering
    /// property is why this is a scoped ambient flag and not a
    /// `mark_leaf(class, method, descriptor)` post-pass: a post-pass keyed by
    /// triple would keep claiming leafness for whatever body happened to win
    /// the slot last.
    ///
    /// Prefer [`with_leaf`](Self::with_leaf) for a scoped set/restore.
    pub fn set_leaf(&mut self, leaf: bool) {
        self.current_leaf = leaf;
    }

    /// The leaf claim currently applied to new registrations.
    pub fn current_leaf(&self) -> bool {
        self.current_leaf
    }

    /// Run `f` with the leaf claim set, restoring the previous value after.
    /// See [`set_leaf`](Self::set_leaf) for the contract `f`'s registrations
    /// are asserting.
    pub fn with_leaf(&mut self, leaf: bool, f: impl FnOnce(&mut Self)) {
        let prev = self.current_leaf;
        self.current_leaf = leaf;
        f(self);
        self.current_leaf = prev;
    }

    /// [`Self::register`], with the kind stated **at the registration site**
    /// instead of inherited from whatever `set_category` the enclosing
    /// registrar last ran.
    ///
    /// This is the migration target for
    /// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.
    /// `register` takes four arguments, none of them a kind; the kind comes
    /// from a mutable field on the registry that some *ancestor* frame set. The
    /// consequence runs in both directions and is silent at the point of the
    /// mistake:
    ///
    /// * one `set_category(Bridge)` line at the head of
    ///   `register_collections_natives` tags **1,195 registrations**, not one
    ///   of which targets an `ACC_NATIVE` method;
    /// * and a permanent bridge that inherits `SyntheticStub` is *dropped* by
    ///   the arms below under `CRATONVM_NO_STUBS` / `JdkOnly`, which is the
    ///   2026-07-14 `java.util.Properties` regression that surfaced minutes
    ///   later as `InternalError: null property: java.home`.
    ///
    /// A registration made through this entry point records
    /// [`NativeCensusEntry::kind_stated`], so the census can finally separate
    /// *"someone adjudicated this"* from *"this inherited the default"* —
    /// which is the evidence the 157-entry reclassification was blocked on.
    ///
    /// `#[track_caller]` on both this and `register` so provenance still points
    /// at the registrar, not at this line.
    #[track_caller]
    pub fn register_with_kind(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
        kind: NativeKind,
    ) {
        // Set/restore rather than passing `kind` down: `register`'s body reads
        // `current_category` in a dozen places (the two drop arms and the
        // `keep_real_*` heuristics), and threading a parameter through all of
        // them would leave the ambient field authoritative for some of the
        // decisions and the argument for others — exactly the split this entry
        // point exists to remove.
        let prev = self.current_category;
        let prev_stated = self.next_kind_stated;
        self.current_category = Some(kind);
        self.next_kind_stated = true;
        self.register(class_name, method_name, descriptor, callback);
        self.current_category = prev;
        self.next_kind_stated = prev_stated;
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

    /// Every class a native for `(method_name, descriptor)` is registered on.
    ///
    /// The registry is keyed on the EXACT class name, and every lookup path
    /// asks it that way — `find`, `resolve_id`, the JIT site cache. This is the
    /// one question they cannot answer: *given a method, which receivers would
    /// take a native?* The JIT's `final`-method devirtualiser needs exactly
    /// that, because it decides at COMPILE time, with no receiver in hand, to
    /// bind a site straight to the classfile body — and a native registered on
    /// a SUBCLASS of the declaring class shadows that body for every receiver
    /// of the subclass. See `invoke::invokevirtual_site_final_owner`, whose
    /// screen this serves, and the netty
    /// `channeloutboundbuffer-close-ordering` page for the two defects that
    /// escaped through the gap.
    ///
    /// Cold path: called once per candidate call site while compiling, never
    /// per execution. The index behind it is built on first use and rebuilt if
    /// `registrations` has grown since (registration is an init-time activity,
    /// so in an ordinary run it is built exactly once, and the length check is
    /// what keeps a late `register()` — the tests do this — from being invisible
    /// to a cached answer).
    #[must_use]
    pub fn owner_classes_for_method(&self, method_name: &str, descriptor: &str) -> Vec<Box<str>> {
        type Index = std::collections::HashMap<(Box<str>, Box<str>), Vec<Box<str>>>;
        let mut guard = self
            .method_owner_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale = match &*guard {
            Some((built_len, _)) => *built_len != self.registrations.len(),
            None => true,
        };
        if stale {
            let mut index: Index = std::collections::HashMap::new();
            for (class, method, desc) in &self.registrations {
                index
                    .entry((method.clone(), desc.clone()))
                    .or_default()
                    .push(class.clone());
            }
            *guard = Some((self.registrations.len(), index));
        }
        let Some((_, index)) = &*guard else {
            return Vec::new();
        };
        index
            .get(&(Box::from(method_name), Box::from(descriptor)))
            .cloned()
            .unwrap_or_default()
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

    /// Schema-v2 census (`docs/feature-designs/jdk-only-mode.md` §4): every
    /// accepted registration with its provenance and dispatch count. Order
    /// follows registration order; callers sort for diff-stable output.
    ///
    /// This is the slow, cold, allocating counterpart to
    /// [`dump_registrations`](Self::dump_registrations) — one `String` per
    /// field per row, ~3,100 rows. It runs at most once per VM, from a CLI dump
    /// or the CI gate. Nothing on any hot path calls it.
    ///
    /// **Superseded rows.** A triple registered twice yields two rows, in
    /// registration order, but only ONE slot (re-registration updates in
    /// place). The one-pass `reg_index -> slot` reverse map below decides which
    /// row owns the counter: the current owner reports the triple's full count,
    /// every superseded row reports `0`. Splitting the count between them is
    /// impossible — the counter never knew about the handover — and attributing
    /// it to both would double the gate's `synthetic_stub_invocations` total.
    pub fn census(&self) -> Vec<NativeCensusEntry> {
        // Reverse of `NativeSlot::reg_index`. One pass over `slots`
        // (<= registrations.len()), then O(1) per row — not a scan per row.
        let mut owner_slot: Vec<Option<u32>> = vec![None; self.registrations.len()];
        for (slot_idx, slot) in self.slots.iter().enumerate() {
            if let Some(cell) = owner_slot.get_mut(slot.reg_index as usize) {
                *cell = Some(slot_idx as u32);
            }
        }
        self.registrations
            .iter()
            .enumerate()
            .map(|(reg_index, (class, name, descriptor))| {
                let prov = self.provenance.get(reg_index);
                let owning_slot = owner_slot.get(reg_index).copied().flatten();
                let invocations = owning_slot
                    .and_then(|slot_idx| self.slot_invocations.get(slot_idx as usize))
                    .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(0);
                NativeCensusEntry {
                    class: class.to_string(),
                    name: name.to_string(),
                    descriptor: descriptor.to_string(),
                    // `categories` is index-parallel with `registrations`; the
                    // fallback is unreachable and picks the conservative kind
                    // (matching `current_category`'s default) so a hypothetical
                    // desync can only ever over-report stubs, never hide one.
                    kind: self
                        .categories
                        .get(reg_index)
                        .copied()
                        .unwrap_or(NativeKind::SyntheticStub),
                    // The one and only place a provenance string is formatted.
                    registered_by: prov.map(|p| format!("{}:{}", p.site.file(), p.site.line())),
                    overwrote: prov.and_then(|p| p.overwrote),
                    invocations,
                    // Read through the same `owner_slot` reverse index as the
                    // count itself, so the flag always describes the slot the
                    // number came from. A superseded row reports `0`
                    // invocations and therefore `true`: a floor of zero on a
                    // row that can never be dispatched is exact, and claiming
                    // it might be higher would invent a doubt.
                    invocations_complete: owning_slot
                        .and_then(|slot_idx| {
                            self.slot_invocations_incomplete.get(slot_idx as usize)
                        })
                        .map(|f| !f.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(true),
                    // Same index-parallel discipline (and same conservative
                    // fallback direction) as `kind` above: a hypothetical
                    // desync reports "inherited", never a false "adjudicated".
                    kind_stated: self.kind_stated.get(reg_index).copied().unwrap_or(false),
                    // Same index-parallel discipline. The conservative fallback
                    // here is `true`, not `false`: this column exists to count
                    // rows nobody chose, so a desync must never invent one.
                    kind_chosen: self
                        .category_chosen_log
                        .get(reg_index)
                        .copied()
                        .unwrap_or(true),
                    // The same `owner_slot` reverse index the invocation count
                    // above is read through — a superseded registration has no
                    // entry in it, which is precisely what "owns no slot" means.
                    owns_slot: owning_slot.is_some(),
                }
            })
            .collect()
    }

    /// Every registration in this registry that a LATER `register*` of the
    /// identical triple displaced — the losers of last-write-wins.
    ///
    /// See [`shadowed_registrations_in`] for what this is for and why it is the
    /// only mechanical way to find this defect species. Cold: it censuses.
    pub fn shadowed_registrations(&self) -> Vec<ShadowedRegistration> {
        shadowed_registrations_in(&self.census())
    }

    /// Permit the synthetic `java.net.Socket` / `ServerSocket` natives to be
    /// registered even under `CRATONVM_REAL_NET_SOCKETS`. See
    /// [`Self::allow_synthetic_net_sockets`]. Test-registry use only.
    pub fn allow_synthetic_net_sockets(&mut self, allow: bool) {
        self.allow_synthetic_net_sockets = allow;
    }

    /// Register a native method implementation.
    ///
    /// `#[track_caller]` (2026-07-31, JDK-only §4): the ~40 `register_*`
    /// functions across `native-builtins` / `native-collections` / … are the
    /// only thing that knows where a stub is declared, and after boot that
    /// information was gone. The attribute is a caller-ABI change with no
    /// signature change — every call site keeps compiling — and costs the
    /// caller two words of `&'static Location` at the call, not a `format!`.
    /// The string is built only if [`census`](Self::census) or the JDK-only
    /// refusal path actually needs it.
    ///
    /// # The one kind decision made here rather than at the site
    ///
    /// A `Bridge` whose receiver class no supported JDK image declares cannot
    /// bind to an `ACC_NATIVE` method — §1.5's whole definition — so it is
    /// re-tagged [`NativeKind::SyntheticStub`] before any of the policy arms
    /// below run, and reports `kind_stated`, because a measurement adjudicated
    /// it. See [`crate::no_image_receiver`] for the measurement, the six images
    /// it was taken against, and why the reviewed VM services are excluded.
    ///
    /// Ordering is load-bearing: the re-tag has to happen before the `JdkOnly`
    /// arm, or strict mode keeps admitting exactly the rows the re-tag exists to
    /// exclude.
    #[track_caller]
    pub fn register(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
    ) {
        // Two measured, centrally-applied kind decisions, in one arm because
        // they have the same shape and the same reason: a fact about the JDK
        // image that no registration site can know, adjudicated once.
        //
        //  * the receiver class no supported image declares (§1.5 has no target
        //    to bind to), and
        //  * a triple RETIRED as a §1.4 shadow, subsystem by subsystem, each
        //    against a strict-corpus measurement. See `crate::retired_shadow`.
        if self.effective_category() == NativeKind::Bridge
            && (crate::no_image_receiver::receiver_declared_by_no_supported_image(class_name)
                || crate::retired_shadow::triple_is_retired_shadow(
                    class_name,
                    method_name,
                    descriptor,
                ))
        {
            let prev = self.current_category;
            let prev_stated = self.next_kind_stated;
            self.current_category = Some(NativeKind::SyntheticStub);
            self.next_kind_stated = true;
            self.register_inner(class_name, method_name, descriptor, callback);
            self.current_category = prev;
            self.next_kind_stated = prev_stated;
            return;
        }
        self.register_inner(class_name, method_name, descriptor, callback);
    }

    /// [`register`](Self::register)'s body. Split out only so the re-tag above
    /// can set/restore around it: this function has a dozen early returns, and
    /// restoring at each of them is the shape that eventually misses one.
    #[track_caller]
    fn register_inner(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
    ) {
        // JDK-only strict policy (docs/feature-designs/jdk-only-mode.md §4,
        // wave 1, 2026-07-31). Deliberately placed BEFORE the
        // `drop_synthetic_stubs` arm below rather than folded into it: the two
        // answer different questions and must stay separable. `CRATONVM_NO_STUBS`
        // is an operator's blunt "make the whole SyntheticStub bucket vanish"
        // switch and is silent by design; `JdkOnly` is a *policy* that has to
        // produce attributed, structured evidence (§10 is "measurement, not
        // deletion"). Merging them would mean either the env var suddenly starts
        // allocating a violation per drop, or JdkOnly loses its provenance —
        // and the arm below is load-bearing for a documented real-JDK bootstrap
        // regression (java.home), so it is left byte-for-byte alone.
        //
        // Compatible mode pays exactly one field load plus a discriminant
        // compare here, and the `allowed_in` call is short-circuited away.
        if self.compatibility_mode == CompatibilityMode::JdkOnly
            && !self
                .effective_category()
                .allowed_in(CompatibilityMode::JdkOnly)
        {
            // Reuse the existing `CRATONVM_DBG_DROPPED_STUBS` switch (added
            // 2026-07-14 for the mis-tagged-category bisection) with a distinct
            // tag, so an operator debugging "why did this native disappear" sets
            // ONE env var and sees both mechanisms. The tags differ because the
            // remedies differ: `[DROPPED-STUB]` means "unset CRATONVM_NO_STUBS",
            // `[JDK-ONLY-REFUSED]` means "this stub is a real JDK-only blocker,
            // go implement or re-tag it".
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DROPPED_STUBS").is_some() {
                eprintln!("[JDK-ONLY-REFUSED] {class_name}.{method_name}{descriptor}");
            }
            // This is the one place a provenance string is built eagerly, and it
            // is bounded by the number of refusals (which the gate drives to
            // zero), not by the ~3,100 registrations.
            let site = core::panic::Location::caller();
            // The refusal is only a RETIREMENT when nothing already owns the
            // triple. See `JdkOnlyViolation::SyntheticNativeRegistered`'s
            // `survivor` doc: an earlier registration survives the refusal and
            // keeps serving, so strict mode runs that older native rather than
            // the real bytecode the policy was asking for.
            let survivor = self.surviving_owner(class_name, method_name, descriptor);
            self.refused
                .push(JdkOnlyViolation::SyntheticNativeRegistered {
                    class: class_name.to_string(),
                    method: method_name.to_string(),
                    descriptor: descriptor.to_string(),
                    registered_by: Some(format!("{}:{}", site.file(), site.line())),
                    survivor,
                });
            // Return WITHOUT inserting: nothing is pushed to `registrations` /
            // `categories` / `provenance` / `slots`, so the refused triple never
            // appears in the census and `generation()` does not move.
            return;
        }
        // Strict no-stubs mode: drop synthetic-stub registrations entirely so
        // the call falls through to real bytecode or a clear error instead of a
        // fake. Bridges and intrinsics are always registered. (See the
        // `drop_synthetic_stubs` field doc.)
        if self.drop_synthetic_stubs && self.effective_category() == NativeKind::SyntheticStub {
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
            && !self.allow_synthetic_net_sockets
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
                    && self.effective_category() == NativeKind::SyntheticStub))
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
            self.effective_category() == NativeKind::Bridge
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
                    // `awaitQuiescence` was in the interpreter's
                    // `is_forkjoin_native_override` list but NOT here, so the
                    // registration was dropped and the interpreter's "force the
                    // native" had no native to force: the call fell through to
                    // real bytecode and answered "quiescent" immediately while
                    // an `execute(Runnable)` daemon thread was still running.
                    // A `--dump-native-registry` census (no awaitQuiescence
                    // row) plus a probe observing `execute(slow);
                    // awaitQuiescence()` with ran=0 where HotSpot reports
                    // ran=1 is what surfaced it. The two lists must agree
                    // entry-for-entry — a name present in only one of them is
                    // silently inert.
                    | ("awaitQuiescence", "(JLjava/util/concurrent/TimeUnit;)Z")
                    // BULK SUBMISSION — see the matching block in
                    // `is_forkjoin_native_override`. `invokeAll(Collection)` is
                    // the overload Weld's `ConcurrentBeanDeployer` calls and was
                    // on neither list, which failed the entire hibernate
                    // `org.hibernate.orm.test.cdi.*` cluster. `invokeAny` was
                    // uncovered too and failed SILENTLY (ran the callables,
                    // returned null); `lazySubmit` threw like invokeAll.
                    | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/List;")
                    | (
                        "invokeAll",
                        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/util/List;",
                    )
                    | (
                        "invokeAllUninterruptibly",
                        "(Ljava/util/Collection;)Ljava/util/List;",
                    )
                    | ("invokeAny", "(Ljava/util/Collection;)Ljava/lang/Object;")
                    | (
                        "invokeAny",
                        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
                    )
                    | (
                        "lazySubmit",
                        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
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
        let keep_real_forkjointask_bridge = self.effective_category() == NativeKind::Bridge
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
                    // `(status & ABNORMAL) != 0` over the real `status` field,
                    // which no Bridge here ever writes — completions live in
                    // the `fjp_state` side table. Real bytecode therefore
                    // answered `false` for a task that had just raised
                    // ExecutionException (RJdkForkJoin.java:259). Must stay in
                    // step with `is_forkjoin_native_override`.
                    | ("isCompletedAbnormally", "()Z")
                    | ("isCancelled", "()Z")
                    | ("cancel", "(Z)Z")
                    | ("complete", "(Ljava/lang/Object;)V")
                    // Reads the throwable the side table records for an
                    // abnormally completed task, so it cannot disagree with
                    // join()/get() about whether the task failed.
                    | ("getException", "()Ljava/lang/Throwable;")
                    // W6-9: the WRITER for that record. Unregistered, real
                    // bytecode CASed the real `aux`/`status`, which nothing
                    // here reads — the task stayed `done == false` and the next
                    // `join()` ran its body and returned a value. Must stay in
                    // step with `is_forkjoin_native_override`.
                    | ("completeExceptionally", "(Ljava/lang/Throwable;)V")
                    // W6-9 §7.2: the only method on the class that CLEARS a
                    // status bit. Unregistered, real bytecode reset the real
                    // `status`/`aux`, which nothing here reads — the side-table
                    // completion survived and the next `join()` replayed the
                    // stale result. Must stay in step with
                    // `is_forkjoin_native_override`.
                    | ("reinitialize", "()V")
                    // L12: the STATIC `invokeAll` overloads. JDK 25's
                    // `invokeAll(t1, t2)` runs one task inline and then blocks
                    // in `awaitDone` for the FORKED sibling — which the lazy
                    // `fork()` above never schedules and no worker thread
                    // exists to run. RJdkForkJoin hung there forever.
                    | (
                        "invokeAll",
                        "(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V",
                    )
                    | ("invokeAll", "([Ljava/util/concurrent/ForkJoinTask;)V")
                    | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/Collection;")
                    // W6-7: the `quietly*` family. `quietlyJoin()` is
                    // `if (status >= 0) awaitDone(false, 0L);` and
                    // `quietlyInvoke()` is `doExec(); ... awaitDone(...)` over
                    // the real `status` field, which no Bridge here ever writes
                    // — completions live in the `fjp_state` side table. With no
                    // worker threads that is a HANG, not a null. Must stay in
                    // step with `is_forkjoin_native_override`, entry for entry.
                    | ("quietlyJoin", "()V")
                    | ("quietlyInvoke", "()V")
                    | ("quietlyComplete", "()V")
                    | ("quietlyJoin", "(JLjava/util/concurrent/TimeUnit;)Z")
                    | ("quietlyJoinUninterruptibly", "(JLjava/util/concurrent/TimeUnit;)Z")
                    | ("quietlyJoinPoolInvokeAllTask", "(J)V")
                    // The two STATIC accessors that answer "which pool am I
                    // in". Registered by `phases_early.rs`, backed by
                    // `FJP_POOL_STACK`; unregistered they run real bytecode
                    // that asks `Thread.currentThread() instanceof
                    // ForkJoinWorkerThread`, which is false on this VM even
                    // inside `pool.invoke` because the pool runs its tasks
                    // INLINE. Adding a registration without adding it to THIS
                    // list is silent: the filter below drops any Bridge on
                    // these three classes that the list does not name, so the
                    // native is registered and then thrown away. Measured that
                    // way first. Must stay in step with
                    // `is_forkjoin_native_override`.
                    | ("inForkJoinPool", "()Z")
                    | ("getPool", "()Ljava/util/concurrent/ForkJoinPool;")
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
        // bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md
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
        // Real-JDK mode: drop the synthetic `java/io/StringWriter` surface.
        // It models the writer as `char[] buf` (slot 0) + `int count` (slot 1).
        // The real JDK layout is the inherited `Writer.lock` (slot 0, Object)
        // and `StringWriter.buf` (slot 1, StringBuffer), which the JDK holds
        // EQUAL to each other. The synthetic state squats two real
        // reference-typed fields, so a SUBCLASS reading the inherited
        // `this.lock` saw an `int[]` where the JDK guarantees the StringBuffer
        // — the assertion `regression-suite`'s `RSerial` fails on. A moving GC
        // tracing a `char[]` parked in that mislabelled slot is the same hazard.
        //
        // The registration already sits behind `#[cfg(feature = "synthetic-jdk")]`
        // with the comment "let the real bytecode run by default", but a Cargo
        // feature only decides what is COMPILED: a feature-enabled binary RUN
        // in real-JDK mode still registered it over the real class. This is the
        // runtime half of that same intent. It is the only production
        // registration site for the class (the others are `#[cfg(test)]`), so
        // dropping it here reproduces the default build exactly, and the real
        // StringWriter bytecode is self-contained — it just delegates to a real
        // StringBuffer.
        if self.drop_real_layout_synthetic && class_name == "java/io/StringWriter" {
            return;
        }
        // Real-JDK mode: drop the synthetic CHARSET family.
        //
        // These natives fabricate their objects with the ABSTRACT class as the
        // runtime class — `alloc_concurrent_synthetic("java/nio/charset/Charset", …)`
        // and the same for `CharsetEncoder`. On HotSpot `StandardCharsets.UTF_8`
        // is a `sun.nio.cs.UTF_8` and its encoder a `sun.nio.cs.UTF_8$Encoder`;
        // here both were instances of the abstract classes themselves. Any
        // method with no native to intercept it then resolves to an abstract
        // declaration, and the ones that WERE intercepted read a synthetic
        // 3-slot layout (`charset`/`averageBytesPerChar`/`maxBytesPerChar` at
        // field indices 0/1/2) that a real coder does not have.
        //
        // The visible result was silent and wrong rather than an error: EVERY
        // `String.getBytes` overload — no-arg, `(String)`, `(Charset)`, for
        // UTF-8, ASCII and ISO-8859-1 alike — returned a correctly-SIZED,
        // ZERO-FILLED array, and every decode round-trip failed with it.
        //
        // That is what made `regression-suite`'s `RCrypto` look like a crypto
        // defect. It is not: the SHA-256 was computed correctly over the wrong
        // input. `"abc".getBytes("UTF-8")` handed it three zero bytes, and
        // sha256(00 00 00) is exactly the 709e80c8… digest the suite reported.
        // `RStrings` failed the same way on its UTF-8 round-trip, so the two
        // classes were one defect.
        //
        // All four names have to go together, and the order in which they were
        // added is the evidence for that: dropping the coders alone moves the
        // failure from zeros to `AbstractMethodError: CharsetEncoder.encodeLoop
        // has no Code attribute`, because the Charset handing out the encoder
        // is still a fabricated abstract instance; adding `Charset` fixes the
        // no-arg and named overloads but leaves `getBytes(StandardCharsets.UTF_8)`
        // failing on `Charset.newEncoder`, because the STANDARD CHARSET OBJECT
        // is fabricated by its own registrations. With all four dropped the real
        // `sun.nio.cs` classes are constructed and every overload matches
        // HotSpot.
        //
        // Verified not to touch the shipping build: the default `cratonvm-cli`
        // build measures the identical 30-passed/1-failed with and without this
        // rule (the one failure, `RSocketChannelInterrupt`, is dev's own and
        // predates it). Synthetic-jdk MODE is byte-identical too —
        // `drop_real_layout_synthetic` is only ever set in a real-JDK arm.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/nio/charset/Charset"
                    | "java/nio/charset/StandardCharsets"
                    | "java/nio/charset/CharsetEncoder"
                    | "java/nio/charset/CharsetDecoder"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `FileChannel.open` FACTORY.
        //
        // `native_fc_open` (native-io) is the THIRD producer of an abstract
        // `java/nio/channels/FileChannel` instance, and the one the suite's
        // `RChannelInterrupt` actually reaches. The other two —
        // `FileSystemProvider.newFileChannel`'s fallback and
        // `RandomAccessFile.getChannel` — were audited first and are not on
        // this path; that audit is why this took two rounds to find.
        //
        // It does `alloc_object(FileChannel, 2)` and writes an fd id and a
        // position into slots 0/1. So `FileChannel.open(p, WRITE).getClass()`
        // was `java.nio.channels.FileChannel` itself where HotSpot 25 answers
        // `sun.nio.ch.FileChannelImpl`, and every method with no native to
        // intercept it resolved to an ABSTRACT declaration:
        // `AbstractMethodError: FileChannel.write(Ljava/nio/ByteBuffer;J)I has
        // no Code attribute`. Only the no-position `write(ByteBuffer)` had a
        // native at all. The same object also has no `interruptor`, which is
        // the field `AbstractInterruptibleChannel.begin()` dereferences — the
        // very defect `RChannelInterrupt` was written for.
        //
        // It is also wrong in a quieter way: the implementation is documented
        // "simplified" and calls `open_read`, IGNORING the `OpenOption[]`
        // entirely. `FileChannel.open(p, WRITE)` handed back a READ-ONLY fd.
        //
        // Dropping it is the whole fix because the real path is already built
        // and already forced: `FileChannel.open` bytecode calls
        // `FileSystemProvider.newFileChannel`, which is force-listed in
        // `native_override.rs` and routed to the base-class registration, and
        // that shim's RECONCILE-WITH-REAL block constructs a genuine
        // `sun.nio.ch.FileChannelImpl` via its 7-arg `open`. Instrumenting that
        // block previously produced NO output on this path — because
        // `native_fc_open` intercepted the call before the provider was ever
        // consulted. Its synthetic fallback stays in place, so a host where the
        // real construction fails keeps exactly today's behaviour.
        //
        // Scoped to `open` BY NAME. The instance natives on this class
        // (`read`/`write`/`position`/`size`/`close`) still serve that fallback
        // object; they do not intercept a real `FileChannelImpl`, whose own
        // declarations win because native dispatch keys on the resolved
        // method's declaring class.
        if self.drop_real_layout_synthetic
            && class_name == "java/nio/channels/FileChannel"
            && method_name == "open"
        {
            return;
        }
        // Real-JDK mode: drop the synthetic blocking/concurrent QUEUE family.
        // Every one of these is registered with the native-collections
        // four-slot layout (array/head/size/capacity), which no real JDK class
        // in the family has: `LinkedBlockingQueue` is
        // head/last/count/putLock/takeLock/notEmpty/notFull/capacity,
        // `ArrayBlockingQueue` is items/takeIndex/putIndex/count/lock/notEmpty/
        // notFull, and the `ConcurrentLinked*` pair is a CAS-linked node chain
        // with no lock fields at all. The synthetic `<init>` never assigns the
        // real ones, so the real bytecode NPEs on them the moment it runs.
        //
        // Only the Deque was dropped here originally, which left the other four
        // half-native: `offer` returned `true` into the side layout while
        // `size()` read 0 and the real `poll(timeout)`/`take()` bytecode NPE'd
        // on a null `takeLock`. That is silent, and it is load-bearing —
        // `ThreadPoolExecutor`'s work queue IS a `LinkedBlockingQueue`, so
        // `execute()` queued every task past `corePoolSize` into a store no
        // worker could see. Exactly `corePoolSize` tasks ran, the rest were
        // lost with no exception, and the pool never reached TERMINATED
        // (`newSingleThreadExecutor` resolved 1 of 6 futures). The real bodies
        // are self-contained — ReentrantLock/Condition for the blocking pair,
        // CAS for the ConcurrentLinked pair — so dropping the surface is all
        // that is needed.
        //
        // This only ever bit a binary built with the `synthetic-jdk` Cargo
        // feature and RUN in real-JDK mode: the default `cratonvm-cli` build
        // does not compile these registrations at all. That is precisely the
        // configuration the vm test gate and the regression suite use, so the
        // damage was to measurement rather than to shipped behaviour — seven
        // regression-suite classes were red for this reason alone.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/util/concurrent/LinkedBlockingDeque"
                    | "java/util/concurrent/LinkedBlockingQueue"
                    | "java/util/concurrent/ArrayBlockingQueue"
                    | "java/util/concurrent/ConcurrentLinkedQueue"
                    | "java/util/concurrent/ConcurrentLinkedDeque"
                    | "java/util/concurrent/BlockingQueue"
            )
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
        let keep_real_scheduled_executor_bridge = self.effective_category() == NativeKind::Bridge
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
        // Real-JDK mode: drop the `Executors` POOL FACTORIES so the real
        // `java.util.concurrent.Executors` bytecode builds every executor.
        //
        // JDK-ONLY-WAVE2 L10 (`L10-blocker-threadpool-init-DONE-20260806.md`,
        // `docs/jdk-only-runtime-services.md` P1). The scheduled pair has been
        // dropped here since the Tomcat `ContainerBase` fix; the three plain-pool
        // factories were the ones still fabricating. What they did was subtler
        // than a two-slot stub and is worth stating, because the obvious reading
        // of the code says they were already fine:
        //
        //   `alloc_concurrent_synthetic(.., "java/util/concurrent/ThreadPoolExecutor", 2)`
        //   followed by `initialize_real_thread_pool_executor`, which
        //   `invoke_special`s the real `ThreadPoolExecutor.<init>`.
        //
        // On the happy path that does produce a genuinely real executor — which
        // is why the fixed/cached transcripts already matched HotSpot. But it
        // keeps TWO fallbacks (`stpe_legacy_slot_init`, taken when the
        // `BlockingQueue` or the `TimeUnit` constant cannot be built) that write
        // the historical two-slot shape onto a real-layout object and return it
        // as if construction had succeeded. A fallback that silently hands back a
        // half-constructed executor is exactly the state the eight
        // `ThreadPoolExecutor.execute` receiver-shape dispatch sites exist to
        // detect, so leaving it in place would keep the predicate they consult
        // conditionally true — and "conditionally true" is what blocks deleting
        // them (L11).
        //
        // And one of the three was observably wrong, not just fragile:
        // `newSingleThreadExecutor()` returned a bare `ThreadPoolExecutor`, where
        // the real JDK returns `Executors$AutoShutdownDelegatedExecutorService`
        // wrapping one. Every `instanceof ThreadPoolExecutor` a caller writes
        // flipped, and the pool was reconfigurable when the JDK guarantees it is
        // not. Measured against HotSpot 25 by `probes/L10ThreadPoolInitProbe`:
        // `single.class` and `single.isTpe` were the ONLY two divergent lines out
        // of 62 in both `--real-jdk` and `--jdk-only`.
        //
        // Dropping is the whole fix, and it is a registration-time decision on
        // purpose — the L11/item-3 precedent
        // (`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`)
        // is that a policy expressed at dispatch has to be restated once per
        // dispatch path, while a policy expressed here is invisible to all of
        // them at once. With no native, `Executors.newFixedThreadPool` et al. run
        // their own bytecode — `new ThreadPoolExecutor(...)`, or for the single-
        // thread case the delegating wrapper — so a factory-built executor is
        // real by CONSTRUCTION rather than by a fallback that happened not to be
        // taken. `newSingleThreadExecutor(ThreadFactory)` was never registered
        // and has always run that real path, which is what made this safe to do
        // rather than merely desirable.
        //
        // Scoped to the pool factories by NAME. `Executors.callable`,
        // `defaultThreadFactory`, `privilegedThreadFactory` and the
        // `unconfigurable*` wrappers are not fabrications and keep whatever
        // registration they have; and the synthetic-JDK build never sets this
        // flag, so its own two-slot executor model is untouched.
        if self.drop_real_layout_synthetic
            && class_name == "java/util/concurrent/Executors"
            && matches!(
                method_name,
                "newScheduledThreadPool"
                    | "newSingleThreadScheduledExecutor"
                    | "newFixedThreadPool"
                    | "newCachedThreadPool"
                    | "newSingleThreadExecutor"
            )
        {
            return;
        }
        // Real-JDK mode: drop the two `identity()` FACTORIES so `java.base`'s
        // own bytecode runs and the VM spins a real lambda proxy.
        //
        // JDK-ONLY-WAVE2 L18 (`docs/known-issues/jdk-only/L18-function-identity-not-synthetic.md`).
        // `Function.identity()` and `UnaryOperator.identity()` are each a single
        // `invokedynamic` returning `t -> t` (verified with `javap -p -c` against
        // the JDK 25 image), so the class HotSpot answers with is a generated
        // `$$Lambda` hidden class: `isSynthetic()` true, name containing
        // `$$Lambda`. CratonVM intercepted both with a native returning a
        // hand-made `java/util/function/Function$Identity` stand-in — an
        // ordinary named class that fails all three predicates
        // (`RJdkLambdas.java:61,62,66`). `--jdk-only` already drops these
        // (`SyntheticStub`) and passes that block; this makes real-JDK mode
        // agree. The `invokedynamic` opcode is short-circuited in
        // `vm/src/runtime/invokedynamic.rs` and never calls the
        // `LambdaMetafactory` natives, so the proxy-spinning machinery is the
        // same code in both modes and the strict arm's result transfers.
        //
        // Only the FACTORIES. `Function$Identity.{apply,andThen,compose}` keep
        // their registrations and simply become unreachable — dropping the
        // instance methods while one of the two factory copies survived is
        // exactly the 2026-07-14 `d8092acb` regression
        // (`UnsatisfiedLinkError: Function$Identity.andThen` on WildFly boot).
        // Dropping by class+method here covers BOTH factory copies
        // (`native-builtins/src/lib.rs`'s `register_function_identity_natives`
        // and `native-builtins/src/phases_late/streams.rs`) at once, so that
        // split cannot recur. And the synthetic-JDK build never sets this flag,
        // so its stand-in — which has no real bytecode to fall back to — is
        // untouched.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/util/function/Function" | "java/util/function/UnaryOperator"
            )
            && method_name == "identity"
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
        let keep_real_matcher_find_fastpath = self.effective_category() == NativeKind::Intrinsic
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
        let keep_real_pattern_compile_cache = self.effective_category() == NativeKind::Intrinsic
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
            && self.effective_category() != NativeKind::SyntheticStub
        {
            return;
        }
        // Real-JDK mode: `java/lang/String`'s real class bytes are
        // authoritative (contract §1.4), so a `Bridge` registered on it does
        // not survive into a real-JDK registry at all.
        //
        // # This is where the forced-native `String` policy went
        //
        // FOUR hard-coded lists used to decide this at DISPATCH time — a
        // 21-name descriptor-blind arm in `check_override`, a 12-shape
        // inverted whitelist in `force_native_over_real_jdk_bytecode`, a JIT
        // direct bind for `toLowerCase(Locale)`, and
        // `is_jdk_string_charset_name_constructor_override`, which the record
        // that catalogued the first three had missed because it is a
        // differently-named predicate rather than a `String` list. They
        // disagreed with each other, so which implementation of
        // `String.equals`/`hashCode`/`substring` ran depended on how many times
        // the call site had executed. All four are deleted.
        //
        // They are not deleted because they looked redundant. They were
        // MEASURED inert first: a binary with both interpreter lists removed
        // produced a byte-identical 392-case `String` transcript in both modes
        // and identical invocation counts on all 38 exercised `String` registry
        // slots. Neither list ever decided anything, because
        // `resolve_step1_native` (`try_stackless_invoke` step 1) resolves the
        // triple and dispatches whatever it finds before either list runs, and
        // it has no list of its own. Registration was always the real gate;
        // this is that gate, stated once, for every dispatch path.
        //
        // # Why the rule is a KIND and not another method list
        //
        // Adjudicated against the JDK 25 image, 79 of the 80 registrations on
        // `java/lang/String` target a method the image declares with a `Code`
        // attribute — §1.4's `NativeShadowsBytecode` — and exactly one,
        // `intern()`, is genuinely `ACC_NATIVE` and therefore a legitimate
        // §1.5 bridge. So "drop the bridges, keep `intern`" is the adjudication
        // itself rather than a list that has to be kept in step with one.
        //
        // `Intrinsic` survives on purpose: §1.4's reviewed exception. The
        // `CRATONVM_NATIVE_STRING_REGEX` family registers under it
        // (`register_with_kind`, so `kind_stated` is true and the census can
        // see somebody decided), which is what a same-answer-just-faster
        // native is supposed to look like — it wins on kind, with no name
        // anywhere. `SyntheticStub` survives too: it never dispatches under
        // `--jdk-only`, and in compatible mode it is the fallback for a
        // synthetic `String` that has no real class bytes behind it.
        //
        // Anything promoted into this exception needs the evidence, not the
        // intent: `probes/StringPolicyMatrixProbe` diffed against HotSpot, and
        // a measurement that the native is actually faster. The five h2-bnf
        // shapes were argued to be "trivially equivalent to the real bytecode
        // for every input" and were wrong about that for unpaired surrogates,
        // for every out-of-range index's exception message, and for `null`.
        if self.drop_real_layout_synthetic
            && class_name == "java/lang/String"
            && self.effective_category() == NativeKind::Bridge
            && !(method_name == "intern" && descriptor == "()Ljava/lang/String;")
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
        // Real-JDK mode: drop the synthetic `java/lang/ref/ReferenceQueue`
        // CONSTRUCTOR so the real one runs. The synthetic ctor only writes the
        // two-slot (head, size) shape this file's natives use; a real JDK 25
        // `ReferenceQueue` additionally declares `private final Lock lock`,
        // which the real `enqueue`/`poll`/`remove` bytecode synchronizes on.
        // Leaving it null made every real `ReferenceQueue.enqueue()` throw
        // `NullPointerException: Cannot enter synchronized block because
        // "this.lock" is null` — reached from `Reference.enqueue()`, whose
        // native already (correctly) delegates to the real `enqueue` bytecode
        // for a real-layout Reference. Symptom: Spring's
        // `ConcurrentReferenceHashMap` failing to purge stale entries, which
        // aborts `AbstractApplicationContext.resetCommonCaches()` and hence
        // any context refresh that has to be cancelled
        // (`scripting.{bsh,config,groovy}.*` in the Spring suite).
        //
        // Only the constructor is dropped. `poll`/`remove` stay native: the
        // GC's reference processor enqueues by writing the queue's head slot
        // directly (`vm/src/runtime/interpreter.rs`) and never notifies the
        // real `lock`, so real blocking `remove()` bytecode would wait forever.
        if self.drop_real_layout_synthetic
            && class_name == "java/lang/ref/ReferenceQueue"
            && method_name == "<init>"
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
        // (`threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`,
        // `threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md`).
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
        // CAPABILITY GATE (`NativeRegister`). Deliberately the LAST drop arm:
        // every arm above answers "should this native exist at all in this
        // build", which is a compatibility question; this one answers "is this
        // VM permitted to install native code for this triple", which is a
        // security question, and it must see exactly the set of registrations
        // that would otherwise be accepted.
        //
        // Under the default (no policy installed) this is one `Option`
        // discriminant test per registration and nothing else — the ~3,100
        // boot registrations pay a predicted not-taken branch.
        //
        // A refusal RETURNS WITHOUT INSERTING, like the JDK-only arm: nothing
        // reaches `registrations` / `categories` / `provenance` / `slots`, so
        // the refused triple never appears in the census and `generation()`
        // does not move. The denial is recorded in the capability audit log,
        // which is where an operator looks for it.
        if let Some(caps) = self.capabilities.as_ref() {
            let request = crate::capability::Capability::native_register(class_name, method_name);
            if caps.check(request).is_err() {
                return;
            }
        }
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
        self.classes_with_natives
            .insert(native_class_hash(class_name));
        // Tag this registration with the current category (see `with_category`).
        // Re-registration under a new category — e.g. promoting a fixed stub to
        // `Intrinsic` — takes effect, matching the previous `insert`-not-
        // -`or_insert` semantics of the removed `category_by_key` map.
        self.categories.push(self.effective_category());
        // Index-parallel with `categories`: was that kind stated here, or
        // inherited? Only `register_with_kind` sets the flag, and only for the
        // duration of its own inner call.
        self.kind_stated.push(self.next_kind_stated);
        self.category_chosen_log
            .push(self.current_category.is_some());
        // Provenance, index-parallel with the two pushes above. `overwrote` is
        // read HERE — before the `match prior_slot` arm below rewrites
        // `slot.kind` in place — because that is the last moment the displaced
        // kind exists anywhere. `register()` is last-registration-wins and the
        // slot keeps no predecessor, so a read after the update would report the
        // new kind as its own predecessor.
        //
        // `Location::caller()` is a compile-time constant threaded in by
        // `#[track_caller]`: no allocation, no formatting. `format!` happens
        // only in `census()`.
        let overwrote = prior_slot.and_then(|idx| self.slots.get(idx as usize).map(|s| s.kind));
        self.provenance.push(RegistrationProvenance {
            site: core::panic::Location::caller(),
            overwrote,
        });
        // Publish into the dense slot table. Re-registration of a key we have
        // already seen UPDATES THE EXISTING SLOT IN PLACE rather than appending
        // a new one: that is what makes a `NativeMethodId` handed out earlier
        // stay valid (and pick up the new callback, matching the documented
        // last-registration-wins behavior of `register`).
        // AN AMBIENT CATEGORY IS "NO OPINION", AND NO-OPINION MUST NOT
        // OVERWRITE AN ADJUDICATED ONE.
        //
        // `current_category` defaults to `SyntheticStub` and is ambient state,
        // so a registrar that never calls `set_category` / `with_category` /
        // `register_with_kind` tags everything it registers a stub by default
        // rather than by anyone's judgement. That is harmless for a fresh
        // triple. It is not harmless on a RE-registration: a later phase that
        // re-registers a triple purely to win last-write-wins on the CALLBACK
        // — which is exactly why `register_service_loader_natives` re-registers
        // `StreamSupport.stream`, saying so in its own comment — silently
        // downgraded the slot's kind as a side effect.
        //
        // Measured 2026-08-05 on a Spring Boot run: of the 72 slots dispatching
        // a `SyntheticStub` over loaded real bytecode, **not one** had
        // `kind_stated`, and **24** had displaced an explicitly-stated `Bridge`
        // or `Intrinsic` — every `StampedLock` method, all of `ServiceLoader`,
        // `CountDownLatch`, seven `Set.of` arities, and `StreamSupport.stream`.
        //
        // The downgrade is not cosmetic: `NativeKind` decides three separate
        // things. `CompatibilityMode::JdkOnly` REFUSES a `SyntheticStub`,
        // `CRATONVM_NO_STUBS` DROPS one, and
        // `synthetic_stub_kind_should_yield_to_real_bytecode` only arbitrates
        // for one. A bridge mis-tagged this way therefore stops dispatching
        // under `--jdk-only` while its stated original would have been allowed.
        //
        // So: keep the prior kind when the incoming registration expressed no
        // opinion and the prior one did. A registrar that genuinely means to
        // reclassify still can — `set_category`, `with_category` and
        // `register_with_kind` all count as choosing, and a chosen kind always
        // wins. `categories[reg_index]` is updated to match so the census row
        // reports the kind that actually took effect rather than the one this
        // registration would have imposed.
        //
        // "Chose it" is `category_chosen`, NOT the census's `kind_stated`.
        // Keying on the latter was the first cut and it was wrong in the
        // mirror-image way: it preserved a `Bridge` over a
        // `with_category(Intrinsic)` re-registration of
        // `java/lang/Float.intBitsToFloat`, because `with_category` is an
        // opinion that `kind_stated` deliberately does not record. The census
        // diff across the change caught it — one kind moved, and it was that
        // one.
        let prior_chosen_kind = prior_slot
            .and_then(|idx| self.slots.get(idx as usize))
            .map(|s| s.reg_index)
            .filter(|ri| {
                self.category_chosen_log
                    .get(*ri as usize)
                    .copied()
                    .unwrap_or(false)
            })
            .and_then(|ri| self.categories.get(ri as usize).copied());
        let category = match prior_chosen_kind {
            Some(prior) if self.current_category.is_none() => {
                if let Some(slot) = self.categories.get_mut(reg_index) {
                    *slot = prior;
                }
                prior
            }
            _ => self.effective_category(),
        };
        // The leaf claim is a property of the CALLBACK being registered, not of
        // the triple: it travels with `current_leaf` exactly like `category`
        // does, so a later phase that re-registers the same triple with a
        // different body silently demotes it to the funnel unless that phase
        // opts in too. See `set_leaf`.
        let leaf = self.current_leaf;
        match prior_slot {
            Some(idx) => {
                if let Some(slot) = self.slots.get_mut(idx as usize) {
                    slot.callback = callback;
                    slot.kind = category;
                    slot.reg_index = reg_index as u32;
                    slot.leaf = leaf;
                }
            }
            None => {
                let idx = self.slots.len() as u32;
                self.slots.push(NativeSlot {
                    callback,
                    kind: category,
                    reg_index: reg_index as u32,
                    leaf,
                });
                // The ONLY place `slot_invocations` grows — it must stay
                // index-parallel with `slots`, and `slots` only ever grows in
                // this arm (the `Some` arm updates in place, which is what keeps
                // an already-issued `NativeMethodId` valid). Keeping the two
                // pushes adjacent is the same discipline the
                // `registrations`/`classes_with_natives` pair documents above.
                self.slot_invocations
                    .push(std::sync::atomic::AtomicU64::new(0));
                // Index-parallel with `slots` for the same reason and by the
                // same rule: this is the only arm that grows the slot table,
                // so it is the only arm that may grow either sidecar. A slot
                // starts out claiming a complete count; only a bypassing
                // dispatch path clears that claim, via
                // `mark_invocations_incomplete`.
                self.slot_invocations_incomplete
                    .push(std::sync::atomic::AtomicBool::new(false));
                self.slot_by_key.insert(key, idx);
            }
        }
        // CAPABILITY: classify the slot once, here, so the dispatch-side gate
        // (`check_dispatch_capability`) is an integer map lookup rather than a
        // per-invocation string match. Populated only when a policy is
        // installed — with none there is nothing to gate, and the map stays
        // empty and unallocated.
        //
        // Kept adjacent to the slot publication above because it is keyed by
        // the slot index, and re-registration of a triple UPDATES the slot in
        // place: recomputing here keeps the classification tracking whichever
        // triple currently owns the slot.
        if self.capabilities.is_some() {
            let slot_index = match prior_slot {
                Some(idx) => idx,
                None => (self.slots.len() - 1) as u32,
            };
            match crate::capability::classify_native(class_name, method_name) {
                Some(kind) => {
                    self.sensitive_slots.insert(slot_index, kind);
                }
                None => {
                    // A re-registration may have replaced a sensitive triple
                    // with a non-sensitive one; drop the stale classification
                    // rather than leaving the slot gated for the wrong reason.
                    self.sensitive_slots.remove(&slot_index);
                }
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
        lookup_census::probe(lookup_census::FIND_WITH_KIND);
        // One prefilter for both halves — see `find`.
        let class_state = self.class_prefilter(class_name)?;
        if let Some(idx) =
            self.slot_index_from_state(class_state, class_name, method_name, descriptor)
        {
            if let Some(slot) = self.slots.get(idx as usize) {
                return Some((slot.callback, slot.kind));
            }
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
        // PERF (2026-08-13, netty `io.netty.buffer` throughput). This is the
        // lookup `try_stackless_invoke`'s step 1 goes through — the one on the
        // interpreter's EVERY-invoke path — and it was the only entry point
        // that never consulted the class prefilter. It hashed the full triple,
        // missed, and then entered the descriptor-quirk path, for classes like
        // `io/netty/buffer/AdaptiveByteBuf` that register nothing at all. A
        // flat profile of `AdaptiveByteBufAllocatorTest` put `slot_for_exact`
        // at 8.2% and `resolve_id_with_descriptor_quirks` — the `#[cold]` arm —
        // at another 1.8%, the largest family in the run.
        //
        // Answering from the prefix state also removes the second walk over the
        // class name that `native_method_hash` did.
        lookup_census::probe(lookup_census::RESOLVE_ID);
        let class_state = self.class_prefilter(class_name)?;
        let key = native_method_hash_from(class_state, method_name, descriptor);
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
        lookup_census::probe(lookup_census::RESOLVE_ID_BY_KEY);
        if let Some(idx) =
            self.slot_index_for_key(key.as_pair(), class_name, method_name, descriptor)
        {
            return Some(NativeMethodId::from_u32(idx));
        }
        // No prefilter here on purpose: the caller already saved the digest,
        // so a class hash would be NEW work on this path, and the quirk arm
        // below opens with its own cheap descriptor precheck.
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

    /// Whether the native behind `id` was registered as a **leaf** — i.e. may
    /// be dispatched through `safe_native_call_leaf` rather than the full
    /// funnel. See [`set_leaf`](Self::set_leaf) for the contract.
    ///
    /// `false` for an unknown handle, which is the safe answer: an unrecognised
    /// id takes the funnel.
    #[inline]
    pub fn is_leaf_id(&self, id: NativeMethodId) -> bool {
        self.slots.get(id.index()).is_some_and(|slot| slot.leaf)
    }

    /// Every registered triple currently claiming leaf status, as
    /// `(class, method, descriptor)`. Cold — for the census and for the test
    /// that pins the leaf set, never for dispatch.
    pub fn leaf_registrations(&self) -> Vec<(&str, &str, &str)> {
        self.slots
            .iter()
            .filter(|slot| slot.leaf)
            .filter_map(|slot| self.registrations.get(slot.reg_index as usize))
            .map(|(c, m, d)| (&**c, &**m, &**d))
            .collect()
    }

    /// Count one dispatch of `id`. Called by every dispatch path **that still
    /// resolves this triple by name or id** immediately before invoking it
    /// (`docs/feature-designs/jdk-only-mode.md` §4).
    ///
    /// # What this counter does NOT see — read before believing a number
    ///
    /// The counter is attached to the *resolution*, not to the call. Every
    /// optimisation this VM has added to the native path since consists of
    /// removing the resolution from the hot path, and each one took the
    /// counter with it. Two families were isolated and causally confirmed on
    /// 2026-08-17 against the release binary built from `783685c34`
    /// (`docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`):
    ///
    /// * **The interpreter's intrinsic table.** Once an invoke cache installs
    ///   a `CachedInvokeTarget::Intrinsic`, the site holds a raw `fn` pointer
    ///   and this registry is never consulted again. MEASURED: 100,000
    ///   `Math.abs` calls report **1**, and the identical run under
    ///   `CRATONVM_DISABLE_INTRINSICS=1` reports **100,000**. Arm-independent
    ///   — it is just as blind under `--nojit`.
    /// * **The JIT's thin direct-call helpers** (`jit_hashmap_put_direct` and
    ///   its siblings). The compiler emits a direct `CALL` to a VM function
    ///   that open-codes the native's semantics with no `NativeMethodId` in
    ///   scope. MEASURED: 100,000 `HashMap.get` calls report **1,873** in the
    ///   JIT arm and **100,001** under `--nojit`, and the JIT figure does not
    ///   move with the workload size or with `CRATONVM_JIT_THRESHOLD` — it is
    ///   frozen at the count reached before the enclosing loop was compiled.
    ///
    /// So `invocations` is a **lower bound on Java-level calls**, and an exact
    /// count of *registry-resolved dispatches*. A zero is not evidence a body
    /// is dead; a small number is not evidence a body is cold. There is one
    /// configuration in which it is exact for everything measured —
    /// `--nojit` with `CRATONVM_DISABLE_INTRINSICS=1` — and that is the
    /// configuration to take a census in.
    ///
    /// A path that knowingly bypasses this counter must say so once, cold, via
    /// [`mark_invocations_incomplete`](Self::mark_invocations_incomplete), so
    /// the census can label the row instead of the reader having to know this
    /// doc comment exists.
    ///
    /// # Cost
    ///
    /// One bounds-checked index into `slot_invocations` and one **relaxed**
    /// `fetch_add`. MEASURED (`rustc -O`, 12 interleaved rounds, medians, a
    /// 12,011-entry counter vector matching this branch's registration count):
    /// **+9.2 ns/call** against a 1.25 ns bounds-checked-index baseline on a
    /// single hot slot, **+8.4 ns/call** spread over 64 slots. Against the
    /// ~141 ns Rust native-call boundary (`G20-1` §5) that is ~6% and
    /// affordable, which is why it sits here. Against a thin direct-call
    /// helper, whose entire reason to exist is to be cheaper than that
    /// boundary, it is not — which is why those helpers are expected to set
    /// the incomplete bit rather than pay this.
    ///
    /// No allocation, no lock, no hashing, no string comparison —
    /// the caller already holds the `NativeMethodId`, so there is nothing left
    /// to resolve. `&self`, because every dispatch path holds only `&` on the
    /// registry; that is why the counter is an atomic and not a `u64`.
    ///
    /// `Relaxed` is correct and deliberate: the census is a *tally*, read once
    /// at the end of the run, and no other memory is published through it.
    /// Anything stronger would put a fence on the native-dispatch path to buy
    /// an ordering nobody reads.
    ///
    /// An out-of-range `id` — a handle from a different registry, the shape
    /// `callback_of`/`kind_of_id` already answer `None` for — is **ignored**,
    /// never a panic. Dispatch must not be able to abort the VM on a stale
    /// memo.
    #[inline]
    pub fn record_invocation(&self, id: NativeMethodId) {
        if let Some(counter) = self.slot_invocations.get(id.index()) {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Dispatches recorded against `id` this run, or `None` if the handle does
    /// not belong to this registry. O(1).
    ///
    /// A **lower bound** on Java-level calls whenever
    /// [`invocations_complete`](Self::invocations_complete) answers
    /// `Some(false)` — see [`record_invocation`](Self::record_invocation) for
    /// the two measured bypass families and for the one configuration in which
    /// this number is exact.
    #[inline]
    pub fn invocations_of_id(&self, id: NativeMethodId) -> Option<u64> {
        self.slot_invocations
            .get(id.index())
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Declare that this slot is dispatched through a path that does **not**
    /// call [`record_invocation`](Self::record_invocation), so its count is a
    /// floor rather than a total.
    ///
    /// Call it **once, cold, at bind time** — where a call site is wired to a
    /// pre-resolved callback — not per dispatch. The whole point of the bit is
    /// that the bypassing paths cannot afford a per-call atomic; paying one to
    /// announce that you are not paying one would be absurd.
    ///
    /// # The sites that owe this call
    ///
    /// Measured 2026-08-17 and recorded in
    /// `docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`.
    /// None of them calls this yet — this method is the landing point the
    /// nominations in that record are written against, and it is deliberately
    /// on the registry rather than in each caller so there is one contract:
    ///
    /// * `vm/src/runtime/interpreter/dispatch_static.rs`, where
    ///   `populate_invoke_cache` installs `CachedInvokeTarget::Intrinsic`, and
    ///   the matching site in `dispatch_virtual.rs`.
    /// * `vm/src/jit/helpers.rs`, where `jit::try_compile` binds a call site to
    ///   `jit_hashmap_put_direct` / `jit_hashmap_get_direct` /
    ///   `jit_concurrent_hashmap_get_direct` /
    ///   `jit_string_latin1_to_lower_direct`.
    /// * `vm/src/vm/vm_exec.rs`'s `invoke_or_native` arms, which resolve with
    ///   `find_with_kind` and hold no id — that one needs a `resolve_id` first
    ///   and is the least attractive of the three.
    ///
    /// # Sticky by design
    ///
    /// Never cleared, including by re-registration of the triple. A
    /// re-registration updates the slot in place, but a call site already bound
    /// to the *previous* callback keeps calling it, so the count keeps being
    /// short. Clearing the bit would make a stale claim of completeness — the
    /// one direction this instrument must not err in.
    ///
    /// An out-of-range `id` is ignored, exactly as in `record_invocation`.
    #[inline]
    pub fn mark_invocations_incomplete(&self, id: NativeMethodId) {
        if let Some(flag) = self.slot_invocations_incomplete.get(id.index()) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Whether [`invocations_of_id`](Self::invocations_of_id) for this slot is
    /// a total (`Some(true)`) or a floor (`Some(false)`); `None` if the handle
    /// does not belong to this registry. O(1).
    ///
    /// `Some(true)` means only that no dispatch path has *declared* itself a
    /// bypass. It is a claim carried by the code, not a proof — read
    /// [`record_invocation`](Self::record_invocation)'s bypass list before
    /// treating it as one.
    #[inline]
    pub fn invocations_complete(&self, id: NativeMethodId) -> Option<bool> {
        self.slot_invocations_incomplete
            .get(id.index())
            .map(|f| !f.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// How many slots have been declared incomplete
    /// ([`mark_invocations_incomplete`](Self::mark_invocations_incomplete)).
    ///
    /// One number a dump header can print so a reader is told the column is a
    /// floor *before* reading the column, rather than after quoting it. Cold:
    /// one relaxed load per slot, run once at report time, the same shape and
    /// the same justification as
    /// [`invocations_of_kind`](Self::invocations_of_kind).
    pub fn slots_with_incomplete_invocations(&self) -> usize {
        self.slot_invocations_incomplete
            .iter()
            .filter(|f| f.load(std::sync::atomic::Ordering::Relaxed))
            .count()
    }

    /// Total dispatches recorded for slots currently classified `kind`
    /// (`docs/feature-designs/jdk-only-mode.md` §4). The CI gate reads this with
    /// `NativeKind::SyntheticStub` and asserts zero.
    ///
    /// # What a zero from this gate does and does not prove
    ///
    /// It is a sum of [`record_invocation`](Self::record_invocation) counters,
    /// so it inherits that method's blind spots exactly: a synthetic stub
    /// dispatched only through the interpreter's intrinsic table or a JIT thin
    /// direct-call helper contributes **nothing** here. The assertion is
    /// therefore sound in one direction only — non-zero is proof a stub ran,
    /// zero is not proof none did.
    ///
    /// **G33-1's reason the gate had not been wrong yet has EXPIRED.** That
    /// record said "neither bypass family currently serves a `SyntheticStub`
    /// (both tables are `java.base` intrinsics and collection fast paths, all
    /// `Bridge` or `Intrinsic`)". A **third** family has landed since —
    /// `vm/src/runtime/interpreter/invoke.rs`'s `UNCOUNTED_STACKLESS_NATIVES`,
    /// the stackless-invoke path that returns `Handled` before the census
    /// increment — and **four of its fourteen triples are registered
    /// `SyntheticStub`**:
    ///
    /// ```text
    /// java/lang/foreign/DowncallHandle  type        ()Ljava/lang/invoke/MethodType;
    /// java/lang/foreign/DowncallHandle  invoke      ([Ljava/lang/Object;)Ljava/lang/Object;
    /// java/lang/foreign/DowncallHandle  invokeExact ([Ljava/lang/Object;)Ljava/lang/Object;
    /// java/lang/foreign/DowncallHandle  invokeBasic ([Ljava/lang/Object;)Ljava/lang/Object;
    /// ```
    ///
    /// MEASURED — `--dump-native-registry`, `9ae371468`, default (`compatible`)
    /// mode: all four `kind = synthetic-stub`, `owns_slot = true`, registered
    /// at `native-builtins/src/phases_late/foreign_ffm.rs:4247…4265`. So a
    /// Panama downcall taken through the stackless path runs a `SyntheticStub`
    /// and contributes **nothing** to this total: the gate reads zero and says
    /// PASS. That is the quiet failure, not the loud one.
    ///
    /// Under `--jdk-only` those four are not registered at all (same dump,
    /// `mode: jdk-only`: `synthetic-stub` count **0**, total 10,691), so the
    /// jdk-only gate is safe **today, by mode** — a fact about what
    /// `foreign_ffm.rs` currently registers, not a property of this method.
    ///
    /// **Do not read a zero from this method on its own.** Use
    /// [`invocations_of_kind_checked`](Self::invocations_of_kind_checked),
    /// which returns the same total alongside
    /// [`incomplete_slots_of_kind`](Self::incomplete_slots_of_kind) so a gate
    /// can tell a *conclusive* zero from an *uninformative* one and go red
    /// rather than quiet. See
    /// `docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`
    /// and
    /// `docs/known-issues/jdk-only/G48-1-the-input-side-hook-and-a-gate-that-could-go-quiet-20260817.md`.
    ///
    /// **Derived, not maintained.** Three per-kind global counters would make
    /// this O(1), but at the price of a SECOND contended atomic RMW on every
    /// native dispatch: `record_invocation`'s per-slot `fetch_add` is spread
    /// across ~3,100 independent cache lines, whereas a per-kind global is one
    /// line every thread in the VM would ping-pong. This scan is ~3,100 relaxed
    /// loads, run once, cold, at report time. The hot path wins.
    ///
    /// **Consequence of deriving it, by design:** the kind comes from the
    /// slot's *current* classification, and a re-registration of the same triple
    /// under a different kind rewrites that slot in place while its counter
    /// keeps accumulating. So a triple registered as `SyntheticStub`, invoked,
    /// then re-registered as `Bridge` reports its pre-promotion invocations
    /// under `Bridge`. That is the honest reading of "which kind is this slot
    /// now", it errs toward under-reporting stubs only in the direction where a
    /// stub was *fixed*, and `census()` still shows the supersession via
    /// `overwrote`.
    pub fn invocations_of_kind(&self, kind: NativeKind) -> u64 {
        self.slots
            .iter()
            .zip(self.slot_invocations.iter())
            .filter(|(slot, _)| slot.kind == kind)
            .map(|(_, counter)| counter.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    }

    /// How many slots currently classified `kind` have declared their
    /// invocation count a floor
    /// ([`mark_invocations_incomplete`](Self::mark_invocations_incomplete)).
    ///
    /// [`slots_with_incomplete_invocations`](Self::slots_with_incomplete_invocations)
    /// narrowed to one kind, and it is the number that makes
    /// [`invocations_of_kind`](Self::invocations_of_kind) readable: **a zero
    /// total means "none ran" only when this is also zero.** Non-zero here
    /// means at least one slot of that kind is wired to a dispatch path that
    /// does not tick the counter, so the total is a floor and a zero total
    /// carries no information at all.
    ///
    /// Same cost and same justification as `invocations_of_kind`: one relaxed
    /// load per slot, cold, at report time.
    pub fn incomplete_slots_of_kind(&self, kind: NativeKind) -> usize {
        self.slots
            .iter()
            .zip(self.slot_invocations_incomplete.iter())
            .filter(|(slot, _)| slot.kind == kind)
            .filter(|(_, flag)| flag.load(std::sync::atomic::Ordering::Relaxed))
            .count()
    }

    /// [`invocations_of_kind`](Self::invocations_of_kind) paired with the one
    /// number that says whether it may be believed.
    ///
    /// This is the form a CI gate must read. The bare total is sound in one
    /// direction only — non-zero proves a stub ran, zero does not prove none
    /// did — and a gate that asserts `== 0` on it inherits that asymmetry
    /// silently: when the instrument goes blind the assertion goes **quiet**,
    /// which is the worst thing a CI check can do. [`KindInvocations::is_conclusive_zero`]
    /// is the predicate that distinguishes "measured none" from "cannot tell",
    /// and [`KindInvocations::is_clean`] is the assertion itself — it refuses
    /// both a non-zero total and an unmeasurable one.
    pub fn invocations_of_kind_checked(&self, kind: NativeKind) -> KindInvocations {
        KindInvocations {
            kind,
            total: self.invocations_of_kind(kind),
            incomplete_slots: self.incomplete_slots_of_kind(kind),
        }
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

    /// Reset the [`lookup_census`] so a measurement covers only what follows.
    /// Cold; exists so a caller can exclude VM boot from the ratio.
    pub fn reset_lookup_census() {
        lookup_census::reset();
    }

    /// The class-name prefilter: `Some(prefix_state)` when this class MIGHT
    /// register a native, `None` when it provably registers none.
    ///
    /// One-sided by construction, in the safe direction. `classes_with_natives`
    /// holds the digest of every registered class name, so a registered class
    /// can never be absent; an unregistered class whose digest collides with a
    /// registered one merely falls through to the full path and misses there.
    ///
    /// Returning the prefix state rather than a bool is what lets the caller
    /// finish the digest without re-walking the class name — see
    /// [`native_class_hash`].
    #[inline]
    fn class_prefilter(&self, class_name: &str) -> Option<(u64, u64)> {
        let class_state = native_class_hash(class_name);
        self.classes_with_natives
            .contains(&class_state)
            .then_some(class_state)
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
        let class_state = self.class_prefilter(class_name)?;
        let key = native_method_hash_from(class_state, method_name, descriptor);
        let idx = self.slot_index_for_key(key, class_name, method_name, descriptor)?;
        self.slots.get(idx as usize)
    }

    /// Retire this registry's `io/netty/internal/tcnative/**` stand-ins,
    /// permanently, for this VM.
    ///
    /// Called from the library-load path the moment Netty's real
    /// `netty_tcnative` `JNI_OnLoad` has run. See the
    /// [`netty_tcnative_muted`](Self) field doc for why the decision cannot
    /// live at a dispatch site.
    pub fn mute_netty_tcnative_stubs(&self) {
        self.netty_tcnative_muted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether [`mute_netty_tcnative_stubs`](Self::mute_netty_tcnative_stubs)
    /// has been called on this registry.
    pub fn netty_tcnative_stubs_muted(&self) -> bool {
        self.netty_tcnative_muted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Slot index for an exact triple, given the class prefix state the caller
    /// already obtained from [`class_prefilter`](Self::class_prefilter).
    ///
    /// Exists so an entry point can prefilter ONCE and still reach both the
    /// exact lookup and the quirk fallback. `find` and `find_with_kind` used to
    /// call `slot_for_exact` (which prefilters) and then, on a miss, a second
    /// prefilter to decide whether the quirk path was worth entering — hashing
    /// the class name twice for every miss on a class that DOES register
    /// natives, which is the common miss on a collection-heavy workload.
    #[inline]
    fn slot_index_from_state(
        &self,
        class_state: (u64, u64),
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<u32> {
        let key = native_method_hash_from(class_state, method_name, descriptor);
        self.slot_index_for_key(key, class_name, method_name, descriptor)
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
            // Retired stand-ins (see `netty_tcnative_muted`). This is the one
            // place every resolution route — `slot_for_exact`, the
            // descriptor-quirk rewrite, and the precomputed-digest
            // `resolve_id_by_key` — turns a digest into a slot, so a single
            // check here cannot be routed around. It sits on the confirmed-hit
            // edge, past the `slot_by_key` probe, so a miss (the overwhelmingly
            // common case) never even loads the flag.
            if self
                .netty_tcnative_muted
                .load(std::sync::atomic::Ordering::Relaxed)
                && class_name.starts_with(NETTY_TCNATIVE_PACKAGE)
            {
                return None;
            }
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
        lookup_census::probe(lookup_census::FIND);
        // One prefilter for both the exact lookup and the quirk fallback: a
        // class that registers nothing cannot be rescued by a descriptor
        // rewrite either, since every variant is looked up under the SAME class
        // name and that class has no key in `slot_by_key` under any descriptor.
        let class_state = self.class_prefilter(class_name)?;
        if let Some(idx) =
            self.slot_index_from_state(class_state, class_name, method_name, descriptor)
        {
            return self.slots.get(idx as usize).map(|slot| slot.callback);
        }

        // AUDIT 2026-05-17 (Fix 4): the compatibility-variants path was
        // previously building a `Vec<String>` on every miss, even for
        // perfectly-formed descriptors that needed no rewriting (which
        // is the common case — a real miss is usually "this native is
        // not implemented", not "the descriptor needed a fixup"). Now
        // we short-circuit: only walk the variant logic when the
        // descriptor *actually* has a quirk worth rewriting. Clean
        // descriptors return `None` with zero allocation.
        let quirked = Self::find_with_descriptor_quirks(self, class_name, method_name, descriptor);
        if quirked.is_none() {
            lookup_census::note_miss(class_name, method_name, descriptor);
        }
        quirked
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
        lookup_census::probe(lookup_census::QUIRKS);
        // Cheap precheck: if the descriptor has none of the quirks the
        // rewrites target, there are no variants to try — bail before
        // touching the allocator.
        //
        // One backward byte pass answers both questions. `descriptor.rfind(')')`
        // went through `core::str::pattern::CharSearcher`, whose UTF-8 reverse
        // search showed up as its own 0.73% line in a flat profile of
        // `AdaptiveByteBufAllocatorTest` — for a byte that is ASCII by
        // definition in a JVM descriptor (JVMS 4.3.3).
        let bytes = descriptor.as_bytes();
        let mut has_whitespace_or_nul = false;
        let mut rparen = None;
        for (i, &b) in bytes.iter().enumerate() {
            if b.is_ascii_whitespace() || b == b'\0' {
                has_whitespace_or_nul = true;
            }
            if b == b')' {
                rparen = Some(i);
            }
        }
        let object_return_quirk = match rparen {
            Some(i) => {
                let ret = &bytes[i + 1..];
                ret.first() == Some(&b'L') && ret.last() != Some(&b';')
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
            if let Some(idx) = self.slot_index_for_key(k, class_name, method_name, candidate_desc) {
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
    ///
    /// Deliberately NOT `#[track_caller]` (2026-07-31, JDK-only §4 provenance).
    /// Propagating the caller here would attribute every aliased row to the
    /// JDBC registrar that asked for the alias, which is a lie: nobody wrote
    /// those `CallableStatement` registrations, this loop synthesized them. Left
    /// off, `Location::caller()` inside `register()` resolves to the
    /// `self.register(...)` line below, so a census row reading
    /// `native-api/src/registry.rs:<alias_class>` is self-describing —
    /// "this native exists because of an alias, delete the source registration
    /// instead".
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
                let callback = self
                    .slot_for_exact(from_class, &method, &descriptor)?
                    .callback;
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
        self.classes_with_natives
            .insert(native_class_hash(victim.0));
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

    /// A re-registration that expresses NO opinion about the kind must not
    /// downgrade one that did.
    ///
    /// `current_category` defaults to `SyntheticStub`, so a registrar that
    /// re-registers a triple purely to win last-write-wins on the CALLBACK —
    /// which several deliberately do — used to demote the slot's kind as a
    /// side effect. That is not cosmetic: `JdkOnly` refuses a `SyntheticStub`,
    /// `CRATONVM_NO_STUBS` drops one, and only a `SyntheticStub` is subject to
    /// the yield-to-real-bytecode arbitration.
    ///
    /// Both directions are asserted. Keeping only the first would pass on an
    /// implementation that froze the kind at its first value and made
    /// `register_with_kind` unable to reclassify anything.
    #[test]
    fn an_unstated_reregistration_does_not_downgrade_a_stated_kind() {
        let desc = "()V";
        let mut r = NativeMethodRegistry::new();

        // Someone adjudicated this as a Bridge.
        r.register_with_kind(
            "java/util/Demo",
            "m",
            desc,
            dummy_native,
            NativeKind::Bridge,
        );
        assert_eq!(
            r.kind_of("java/util/Demo", "m", desc),
            Some(NativeKind::Bridge)
        );

        // A later phase re-registers to win the callback, saying nothing about
        // the kind. The callback must change; the adjudicated kind must not.
        r.register("java/util/Demo", "m", desc, dummy_native_2);
        assert_eq!(
            r.kind_of("java/util/Demo", "m", desc),
            Some(NativeKind::Bridge),
            "an ambient SyntheticStub default must not overwrite a stated Bridge"
        );
        let id = r
            .resolve_id("java/util/Demo", "m", desc)
            .expect("registered");
        let cb = r.callback_of(id).expect("callback");
        let mut ctx = MockNativeContext::new();
        assert_eq!(
            cb(&mut ctx, &[]).expect("call"),
            Some(Value::Int(42)),
            "last-registration-wins on the CALLBACK is unchanged"
        );

        // The other direction: a STATED reclassification still takes effect.
        r.register_with_kind(
            "java/util/Demo",
            "m",
            desc,
            dummy_native,
            NativeKind::SyntheticStub,
        );
        assert_eq!(
            r.kind_of("java/util/Demo", "m", desc),
            Some(NativeKind::SyntheticStub),
            "register_with_kind must still be able to reclassify downward"
        );

        // `set_category` / `with_category` are opinions too, even though the
        // census's `kind_stated` deliberately does not record them. Keying the
        // rule on `kind_stated` preserved a Bridge over a
        // `with_category(Intrinsic)` re-registration of
        // `java/lang/Float.intBitsToFloat` on a real run — the same downgrade
        // this rule exists to stop, pointing the other way.
        let mut chosen = NativeMethodRegistry::new();
        chosen.register_with_kind(
            "java/util/Demo",
            "p",
            desc,
            dummy_native,
            NativeKind::Bridge,
        );
        chosen.with_category(NativeKind::Intrinsic, |r| {
            r.register("java/util/Demo", "p", desc, dummy_native_2);
        });
        assert_eq!(
            chosen.kind_of("java/util/Demo", "p", desc),
            Some(NativeKind::Intrinsic),
            "with_category is a choice and must win over an earlier stated kind"
        );

        // Two registrations that both left the category untouched keep plain
        // last-write-wins — the rule is about opinions, not about order.
        let mut amb = NativeMethodRegistry::new();
        amb.register("java/util/Demo", "n", desc, dummy_native);
        amb.register("java/util/Demo", "n", desc, dummy_native_2);
        assert_eq!(
            amb.kind_of("java/util/Demo", "n", desc),
            Some(NativeKind::SyntheticStub),
            "neither registration chose a kind, so last-write-wins still applies"
        );
    }

    /// The leaf claim is a property of the registered CALLBACK, and a later
    /// phase that re-registers the triple without opting in must take it away.
    ///
    /// This is the whole reason `set_leaf` is a scoped ambient flag rather than
    /// a `mark_leaf(class, method, descriptor)` post-pass. `native-builtins`
    /// genuinely does re-register triples: `register_atomic_integer_natives`
    /// runs twice, and phase 54 installs its own (buggy) atomics in between. A
    /// triple-keyed post-pass would keep claiming "this is a leaf" for whichever
    /// body happened to win the slot last, and the leaf contract — no
    /// allocation, no safepoint, no collection, no JNI exception — would be
    /// asserted about code that never agreed to it.
    #[test]
    fn a_re_registration_that_does_not_opt_in_drops_the_leaf_claim() {
        let mut r = NativeMethodRegistry::new();

        r.set_leaf(true);
        r.register("p/C", "get", "()I", dummy_native);
        r.set_leaf(false);
        let id = r
            .resolve_id("p/C", "get", "()I")
            .expect("registered triple resolves");
        assert!(r.is_leaf_id(id), "the opted-in registration claims leaf");

        // A later phase re-registers the SAME triple with a different body and
        // says nothing about leafness.
        r.register("p/C", "get", "()I", dummy_native_2);
        assert_eq!(
            r.resolve_id("p/C", "get", "()I"),
            Some(id),
            "re-registration updates the slot in place, so the handle stays valid"
        );
        assert!(
            !r.is_leaf_id(id),
            "the new body never asserted the leaf contract, so the claim must be gone"
        );

        // And restating it brings it back, which is what the second
        // `register_atomic_integer_natives` pass relies on.
        r.set_leaf(true);
        r.register("p/C", "get", "()I", dummy_native);
        r.set_leaf(false);
        assert!(r.is_leaf_id(id));
    }

    /// Nothing is a leaf unless it says so, and `with_leaf` restores.
    #[test]
    fn the_leaf_claim_defaults_off_and_is_scoped() {
        let mut r = NativeMethodRegistry::new();
        r.register("p/C", "plain", "()V", dummy_native);
        let plain = r.resolve_id("p/C", "plain", "()V").expect("resolves");
        assert!(!r.is_leaf_id(plain), "default is the full funnel");

        r.with_leaf(true, |r| {
            r.register("p/C", "leafy", "()V", dummy_native);
        });
        let leafy = r.resolve_id("p/C", "leafy", "()V").expect("resolves");
        assert!(r.is_leaf_id(leafy));
        assert!(!r.current_leaf(), "with_leaf restores the previous claim");

        r.register("p/C", "after", "()V", dummy_native);
        let after = r.resolve_id("p/C", "after", "()V").expect("resolves");
        assert!(
            !r.is_leaf_id(after),
            "a registration outside the scope is not leaf"
        );

        let leaves = r.leaf_registrations();
        assert_eq!(leaves, vec![("p/C", "leafy", "()V")]);
    }

    /// A handle from another registry must not read as leaf. `is_leaf_id` is
    /// consulted on a dispatch path that would then skip the funnel's GC
    /// bookkeeping, so "unknown" has to mean "take the funnel".
    #[test]
    fn an_out_of_range_handle_is_not_leaf() {
        let mut a = NativeMethodRegistry::new();
        a.set_leaf(true);
        for i in 0..4 {
            a.register("p/C", &format!("m{i}"), "()V", dummy_native);
        }
        a.set_leaf(false);
        let far = a.resolve_id("p/C", "m3", "()V").expect("resolves");

        let b = NativeMethodRegistry::new();
        assert!(
            !b.is_leaf_id(far),
            "an id that does not index this registry's slots is not a leaf"
        );
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
            r.register(
                "sun/misc/Unsafe",
                "getInt",
                "(Ljava/lang/Object;J)I",
                dummy_native,
            );
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
        assert!(registry
            .find("org/h2/mvstore/MVMap", "get", "()V")
            .is_none());
        assert!(registry
            .kind_of("org/h2/mvstore/MVMap", "put", "()V")
            .is_none());
        assert!(registry
            .find_with_kind("org/h2/mvstore/MVMap", "hashCode", "()I")
            .is_none());
        // …while a registered triple on a registered class still resolves.
        assert!(registry
            .find("java/lang/Object", "hashCode", "()I")
            .is_some());
        // …and an unregistered METHOD on a registered class still misses,
        // which is the prefilter's false-positive path falling through to the
        // full digest + name verification.
        assert!(registry
            .find("java/lang/Object", "toString", "()V")
            .is_none());
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

    /// A retirement must not silently disarm a real-JDK `keep_real_*_bridge`.
    ///
    /// `register` re-tags a retired triple `Bridge` -> `SyntheticStub` BEFORE
    /// calling `register_inner`, and it does so in every mode, because the
    /// table is per-triple. Inside `register_inner`, real-JDK mode keeps a
    /// small number of natives it cannot safely run as bytecode, and each keep
    /// is a predicate over `effective_category() == NativeKind::Bridge` — which
    /// the re-tag has already falsified. A triple that is BOTH retired and
    /// keep-listed therefore loses its native in real-JDK mode, which is not
    /// what a §1.4 shadow retirement is for and is a mode no retiring lane
    /// measures.
    ///
    /// This asks the question of the whole retired population rather than of
    /// one wave, and it asks it through the real code path: for each retired
    /// triple, register it with `register_inner` under `Bridge` — the state
    /// `register` would have been in had the re-tag not run — and require that
    /// real-layout mode drops it anyway. Anything that survives is keep-listed,
    /// and the retirement of it is the bug.
    ///
    /// On 2026-09-10 this caught two: `ScheduledThreadPoolExecutor`'s 3-arg
    /// constructor and `getCorePoolSize()I`, which
    /// `keep_real_scheduled_executor_bridge` holds for Spring's
    /// `ThreadPoolTaskScheduler` anonymous subclass. They were removed from
    /// `RETIRED_SHADOW_L5_TRIPLES` before it landed.
    ///
    /// A failure here does NOT mean "delete the keep arm". It means the triple
    /// cannot be retired by the table, because the table cannot tell the two
    /// modes apart: `drop_real_layout_synthetic` is set in real-JDK AND in
    /// `--jdk-only` (`vm_init.rs`), so there is no flag to branch the re-tag
    /// on. Take the row out of the table.
    #[test]
    fn real_layout_bridge_keeps_are_not_retired_shadows() {
        let mut offenders: Vec<String> = Vec::new();
        for table in crate::retired_shadow::RETIRED_SHADOW_TABLES {
            for (class_name, method_name, descriptor) in table.iter() {
                // Sanity: every row here must actually be reachable through the
                // predicate, or this gate is scoring rows the VM never retires.
                assert!(
                    crate::retired_shadow::triple_is_retired_shadow(
                        class_name,
                        method_name,
                        descriptor
                    ),
                    "{class_name}.{method_name}{descriptor} is in a retired-shadow table but `triple_is_retired_shadow` says no — the class is outside `RETIRED_SHADOW_PREFIXES`"
                );

                let mut real_layout = NativeMethodRegistry::new();
                real_layout.set_drop_real_layout_synthetic(true);
                real_layout.set_category(NativeKind::Bridge);
                // `register_inner`, not `register`: the point is to observe
                // `register_inner`'s own decision with the category the re-tag
                // would have replaced.
                real_layout.register_inner(class_name, method_name, descriptor, dummy_native);
                if real_layout.find(class_name, method_name, descriptor).is_some() {
                    offenders.push(format!("{class_name}.{method_name}{descriptor}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these retired-shadow triples are ALSO kept by a real-JDK `keep_real_*_bridge` arm, so retiring them drops the native in real-JDK mode too — a mode the retiring lane did not measure. Remove them from the table (see this test's doc comment; do not touch the keep arm):
  {}",
            offenders.join("
  ")
        );
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
            (
                "java/lang/System",
                "arraycopy",
                "(Ljava/lang/Object;ILjava/lang/Object;II)V",
            ),
            ("java/lang/String", "length", "()I"),
        ];
        registry.register(triples[0].0, triples[0].1, triples[0].2, dummy_native);
        registry.register(triples[1].0, triples[1].1, triples[1].2, dummy_native_2);
        registry.register(triples[2].0, triples[2].1, triples[2].2, dummy_native);

        for (class, method, descriptor) in triples {
            let by_name = registry
                .find(class, method, descriptor)
                .expect("registered");
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
            registry
                .find_with_kind_by_key(key, "java/lang/String", "length", "()I")
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

        let quirky = "()Ljava/lang/String"; // missing trailing ';'
        let by_name = registry
            .find("q/Q", "m", quirky)
            .expect("quirk rewrite hits");
        let id = registry
            .resolve_id("q/Q", "m", quirky)
            .expect("quirk rewrite yields a handle");
        assert_eq!(
            cb_addr(by_name),
            cb_addr(registry.callback_of(id).expect("redeems"))
        );
        // The handle names the triple actually registered, not the quirky input.
        assert_eq!(
            registry.triple_of(id),
            Some(("q/Q", "m", "()Ljava/lang/String;"))
        );
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
        let fast = registry
            .resolve_id("k/K", "fast", "()I")
            .expect("registered");
        let fake = registry
            .resolve_id("k/K", "fake", "()I")
            .expect("registered");
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

    // -----------------------------------------------------------------------
    // JDK-only wave 2, L10 — real `ThreadPoolExecutor` field initialisation
    // -----------------------------------------------------------------------

    /// Every `Executors` pool factory, and the descriptor CratonVM registered
    /// it under before the L10 drop.
    const EXECUTOR_POOL_FACTORIES: &[(&str, &str)] = &[
        (
            "newFixedThreadPool",
            "(I)Ljava/util/concurrent/ExecutorService;",
        ),
        (
            "newCachedThreadPool",
            "()Ljava/util/concurrent/ExecutorService;",
        ),
        (
            "newCachedThreadPool",
            "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
        ),
        (
            "newSingleThreadExecutor",
            "()Ljava/util/concurrent/ExecutorService;",
        ),
        (
            "newScheduledThreadPool",
            "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        ),
        (
            "newSingleThreadScheduledExecutor",
            "()Ljava/util/concurrent/ScheduledExecutorService;",
        ),
    ];

    /// In real-JDK mode no `Executors` pool factory may be registered: the real
    /// `java.util.concurrent.Executors` bytecode has to build every executor,
    /// so a factory-made pool is genuinely `<init>`-constructed by construction
    /// rather than by a fallback that happened not to be taken.
    ///
    /// That is what unblocks L11. The eight `ThreadPoolExecutor.execute`
    /// receiver-shape dispatch sites exist to detect an executor CratonVM
    /// fabricated; they are deletable only once no such executor can exist, and
    /// a native here is the only thing that could make one.
    ///
    /// Asserted in BOTH directions. Without the `drop_real_layout_synthetic ==
    /// false` half this test passes on a registry that refuses the triples
    /// unconditionally — which would break the synthetic-JDK build, whose
    /// two-slot executor model is exactly what those natives are for, while
    /// this test went on reading green.
    #[test]
    fn real_jdk_mode_registers_no_executors_pool_factory() {
        let exec = "java/util/concurrent/Executors";

        let mut compatible = NativeMethodRegistry::new();
        for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
            compatible.register(exec, name, descriptor, dummy_native);
        }
        for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
            assert!(
                compatible.find(exec, name, descriptor).is_some(),
                "synthetic-JDK mode must KEEP Executors.{name}{descriptor}: it is the \
                 only implementation there, and dropping it would leave the synthetic \
                 two-slot executor model with no factory at all"
            );
        }

        let mut real = NativeMethodRegistry::new();
        real.set_drop_real_layout_synthetic(true);
        for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
            real.register(exec, name, descriptor, dummy_native);
        }
        for (name, descriptor) in EXECUTOR_POOL_FACTORIES {
            assert!(
                real.find(exec, name, descriptor).is_none(),
                "real-JDK mode still registers Executors.{name}{descriptor}. A native here \
                 can hand back an executor the real `<init>` never ran on, which is the \
                 receiver shape the eight `ThreadPoolExecutor.execute` dispatch sites exist \
                 to detect — and while one can exist, those sites cannot be deleted (L11)."
            );
        }

        // Negative control: the drop is scoped to the pool factories by name.
        // `callable`/`defaultThreadFactory`/`unconfigurable*` fabricate no
        // executor and must be unaffected, or a future widening of the
        // `matches!` arm would take them out silently.
        for (name, descriptor) in [
            (
                "callable",
                "(Ljava/lang/Runnable;)Ljava/util/concurrent/Callable;",
            ),
            (
                "defaultThreadFactory",
                "()Ljava/util/concurrent/ThreadFactory;",
            ),
            (
                "unconfigurableExecutorService",
                "(Ljava/util/concurrent/ExecutorService;)Ljava/util/concurrent/ExecutorService;",
            ),
        ] {
            real.register(exec, name, descriptor, dummy_native);
            assert!(
                real.find(exec, name, descriptor).is_some(),
                "the L10 drop must not reach Executors.{name}{descriptor} — it builds no \
                 executor of its own"
            );
        }
    }

    // -----------------------------------------------------------------------
    // JDK-only mode (docs/feature-designs/jdk-only-mode.md §4), 2026-07-31
    // -----------------------------------------------------------------------

    #[test]
    fn allowed_in_rejects_only_synthetic_stubs_under_jdk_only() {
        // §1.3 forbids exactly one kind. Bridges cross VM boundaries and
        // intrinsics are semantics-preserving, so both survive strict mode —
        // pinned here because a well-meaning "strict means fewer natives"
        // tightening of this predicate would silently unboot every real-JDK run.
        for kind in [
            NativeKind::Intrinsic,
            NativeKind::Bridge,
            NativeKind::SyntheticStub,
        ] {
            assert!(
                kind.allowed_in(CompatibilityMode::Compatible),
                "{kind:?} must be allowed in Compatible"
            );
        }
        assert!(NativeKind::Intrinsic.allowed_in(CompatibilityMode::JdkOnly));
        assert!(NativeKind::Bridge.allowed_in(CompatibilityMode::JdkOnly));
        assert!(!NativeKind::SyntheticStub.allowed_in(CompatibilityMode::JdkOnly));
    }

    #[test]
    fn jdk_only_refuses_synthetic_stub_registration_with_provenance() {
        let mut registry = NativeMethodRegistry::new();
        registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
        assert_eq!(registry.compatibility_mode(), CompatibilityMode::JdkOnly);
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("j/J", "fake", "()I", dummy_native);
        });

        // Refused means NOT INSERTED anywhere: no callback, no census row, and
        // `generation()` must not move (a bumped generation would invalidate
        // every live `NativeCallSite` memo for a registration that never
        // happened).
        assert!(registry.find("j/J", "fake", "()I").is_none());
        assert!(registry.census().is_empty());
        assert_eq!(registry.len(), 0);

        match registry.refused_registrations() {
            [JdkOnlyViolation::SyntheticNativeRegistered {
                class,
                method,
                descriptor,
                registered_by,
                survivor,
            }] => {
                assert!(
                    survivor.is_none(),
                    "nothing was registered for this triple before the refusal, \
                     so the refusal really did retire it: {survivor:?}"
                );
                assert_eq!(class, "j/J");
                assert_eq!(method, "fake");
                assert_eq!(descriptor, "()I");
                // `#[track_caller]` must name the `register(...)` call site in
                // THIS file, not `register`'s own body in registry.rs.
                let site = registered_by.as_deref().expect("provenance recorded");
                assert!(
                    site.contains("registry.rs:"),
                    "expected a file:line site, got {site}"
                );
            }
            other => panic!("expected exactly one refusal, got {other:?}"),
        }
    }

    #[test]
    fn jdk_only_keeps_bridges_and_intrinsics_and_compatible_keeps_stubs() {
        let mut strict = NativeMethodRegistry::new();
        strict.set_compatibility_mode(CompatibilityMode::JdkOnly);
        strict.with_category(NativeKind::Bridge, |r| {
            r.register("j/J", "syscall", "()I", dummy_native);
        });
        strict.with_category(NativeKind::Intrinsic, |r| {
            r.register("j/J", "fast", "()I", dummy_native_2);
        });
        assert!(strict.find("j/J", "syscall", "()I").is_some());
        assert!(strict.find("j/J", "fast", "()I").is_some());
        assert!(strict.refused_registrations().is_empty());

        // Compatible is the default and must be unchanged (§1: "--real-jdk
        // (default) keeps today's behaviour exactly"), including the ambient
        // `SyntheticStub` category that `register()` inherits when nobody sets
        // one.
        let mut compatible = NativeMethodRegistry::new();
        assert_eq!(
            compatible.compatibility_mode(),
            CompatibilityMode::Compatible
        );
        compatible.register("j/J", "fake", "()I", dummy_native);
        assert!(compatible.find("j/J", "fake", "()I").is_some());
        assert_eq!(
            compatible.kind_of("j/J", "fake", "()I"),
            Some(NativeKind::SyntheticStub)
        );
        assert!(compatible.refused_registrations().is_empty());
    }

    #[test]
    fn record_invocation_is_per_slot_and_ignores_foreign_ids() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("c/C", "a", "()I", dummy_native);
            r.register("c/C", "b", "()I", dummy_native_2);
        });
        let a = registry.resolve_id("c/C", "a", "()I").expect("registered");
        let b = registry.resolve_id("c/C", "b", "()I").expect("registered");

        assert_eq!(registry.invocations_of_id(a), Some(0));
        registry.record_invocation(a);
        registry.record_invocation(a);
        registry.record_invocation(b);
        assert_eq!(registry.invocations_of_id(a), Some(2));
        assert_eq!(registry.invocations_of_id(b), Some(1));

        // A handle from another registry (or a stale memo) must be a silent
        // no-op, never a panic — this runs on the native-dispatch path.
        let foreign = NativeMethodId::from_u32(9_999);
        registry.record_invocation(foreign);
        assert!(registry.invocations_of_id(foreign).is_none());
        assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), 3);
    }

    #[test]
    fn invocations_are_complete_until_a_bypassing_path_says_otherwise() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("c/C", "counted", "()I", dummy_native);
            r.register("c/C", "bypassed", "()I", dummy_native_2);
        });
        let counted = registry
            .resolve_id("c/C", "counted", "()I")
            .expect("registered");
        let bypassed = registry
            .resolve_id("c/C", "bypassed", "()I")
            .expect("registered");

        // The default is the claim "nothing bypasses this slot", so a fresh
        // registry reports every slot complete and none incomplete. A default
        // of `false` would be honest about the codebase but would make the
        // column useless — every row would carry the same doubt.
        assert_eq!(registry.invocations_complete(counted), Some(true));
        assert_eq!(registry.invocations_complete(bypassed), Some(true));
        assert_eq!(registry.slots_with_incomplete_invocations(), 0);

        registry.record_invocation(bypassed);
        registry.mark_invocations_incomplete(bypassed);

        // The count is still readable and still exact as a FLOOR — marking a
        // slot incomplete must not zero or otherwise disturb the tally, which
        // is the number a reader falls back on.
        assert_eq!(registry.invocations_of_id(bypassed), Some(1));
        assert_eq!(registry.invocations_complete(bypassed), Some(false));
        assert_eq!(
            registry.invocations_complete(counted),
            Some(true),
            "the flag is per slot, not global"
        );
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);

        // Idempotent: a bind site that runs twice (recompilation, a second
        // call site on the same triple) must not be able to change the answer.
        registry.mark_invocations_incomplete(bypassed);
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);

        // Same foreign-handle contract as `record_invocation`: silently
        // ignored, never a panic, and never a fabricated `Some`.
        let foreign = NativeMethodId::from_u32(9_999);
        registry.mark_invocations_incomplete(foreign);
        assert_eq!(registry.invocations_complete(foreign), None);
        assert_eq!(registry.slots_with_incomplete_invocations(), 1);
    }

    #[test]
    fn the_incomplete_flag_survives_re_registration_and_reaches_the_census() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("s/S", "m", "()I", dummy_native);
        });
        let id = registry.resolve_id("s/S", "m", "()I").expect("registered");
        registry.record_invocation(id);
        registry.mark_invocations_incomplete(id);

        // Re-registering the triple updates the slot in place. A call site
        // already bound to the previous callback keeps calling it, so the
        // count keeps being short: clearing the flag here would manufacture a
        // claim of completeness, the one direction this instrument must not
        // err in.
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("s/S", "m", "()I", dummy_native_2);
        });
        assert_eq!(
            registry.invocations_complete(id),
            Some(false),
            "re-registration must not clear the bypass claim"
        );

        let census = registry.census();
        assert_eq!(census.len(), 2, "one row per registration");
        // The superseded row owns no slot, so its zero is exact and it must
        // NOT inherit the doubt: a floor of zero on a row that can never be
        // dispatched is a total.
        assert!(!census[0].owns_slot);
        assert_eq!(census[0].invocations, 0);
        assert!(
            census[0].invocations_complete,
            "a row that owns no slot reports an exact zero"
        );
        // The surviving row carries both the count and the doubt.
        assert!(census[1].owns_slot);
        assert_eq!(census[1].invocations, 1);
        assert!(
            !census[1].invocations_complete,
            "the slot owner must carry the bypass claim into the census"
        );
    }

    #[test]
    fn invocations_of_kind_follows_the_slots_current_kind() {
        // Documents the deliberate consequence of DERIVING per-kind totals by
        // scanning instead of keeping three global counters (which would add a
        // second contended RMW to every native dispatch): a slot re-registered
        // under a different kind carries its accumulated count over to the new
        // kind. Asserted, not merely commented, so a future switch to
        // maintained counters has to confront the behaviour change.
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("p/P", "m", "()I", dummy_native);
        });
        let id = registry.resolve_id("p/P", "m", "()I").expect("registered");
        registry.record_invocation(id);
        registry.record_invocation(id);
        assert_eq!(registry.invocations_of_kind(NativeKind::SyntheticStub), 2);
        assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), 0);

        // Promote the same triple to a bridge: same slot, same counter.
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("p/P", "m", "()I", dummy_native_2);
        });
        assert_eq!(
            registry.resolve_id("p/P", "m", "()I"),
            Some(id),
            "re-registration must update the slot in place"
        );
        assert_eq!(registry.invocations_of_kind(NativeKind::SyntheticStub), 0);
        assert_eq!(registry.invocations_of_kind(NativeKind::Bridge), 2);
    }

    #[test]
    fn a_stub_total_of_zero_is_not_a_pass_when_a_stub_slot_is_declared_incomplete() {
        // The L4 gate is `invocations_of_kind(SyntheticStub) == 0`. This is
        // the state in which that assertion PASSES while proving nothing —
        // the quiet failure G33-1 §4 flagged and this test makes loud.
        //
        // It is not hypothetical. MEASURED on `9ae371468` in default mode:
        // the four `java/lang/foreign/DowncallHandle` arms are registered
        // `synthetic-stub` AND listed in `UNCOUNTED_STACKLESS_NATIVES`, the
        // stackless-invoke bypass family, which returns `Handled` before the
        // census increment. G33-1's "neither bypass family currently serves a
        // SyntheticStub" was true when it was written and is not true now.
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("f/Downcall", "invoke", "()I", dummy_native);
        });
        let stub = registry
            .resolve_id("f/Downcall", "invoke", "()I")
            .expect("registered");
        registry.mark_invocations_incomplete(stub);

        // The bare total: zero. The old gate reads this and says PASS.
        assert_eq!(registry.invocations_of_kind(NativeKind::SyntheticStub), 0);

        // The honest form refuses, and says which of the two failure modes
        // it hit rather than leaving the reader hunting for a stub.
        let checked = registry.invocations_of_kind_checked(NativeKind::SyntheticStub);
        assert_eq!(checked.total, 0);
        assert_eq!(checked.incomplete_slots, 1);
        assert!(!checked.is_measurable());
        assert!(!checked.is_conclusive_zero());
        assert!(
            !checked.is_clean(),
            "a gate must not pass on total == 0 alone"
        );
        assert!(
            checked.describe().contains("proves nothing"),
            "the failure message must name the blindness, not just the count: {}",
            checked.describe()
        );
    }

    #[test]
    fn a_stub_total_of_zero_with_every_stub_slot_counted_is_a_pass() {
        // The other side of the same predicate, so `is_clean` cannot be
        // satisfied by simply never returning true. Marking a slot of a
        // DIFFERENT kind incomplete — which is the true state of every
        // `--jdk-only` run today, where the JIT and interpreter bypass lists
        // resolve only `Bridge` and `Intrinsic` triples — must not make the
        // stub verdict unmeasurable.
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("s/S", "m", "()I", dummy_native);
        });
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("b/B", "m", "()I", dummy_native_2);
        });
        let bridge = registry.resolve_id("b/B", "m", "()I").expect("registered");
        registry.mark_invocations_incomplete(bridge);
        registry.record_invocation(bridge);

        let stubs = registry.invocations_of_kind_checked(NativeKind::SyntheticStub);
        assert!(
            stubs.is_clean(),
            "no stub ran and every stub slot is counted"
        );
        assert_eq!(
            registry.incomplete_slots_of_kind(NativeKind::SyntheticStub),
            0
        );

        // The bridge's own total is a floor, and says so.
        let bridges = registry.invocations_of_kind_checked(NativeKind::Bridge);
        assert_eq!(bridges.total, 1);
        assert!(!bridges.is_measurable());
        assert!(bridges.describe().contains("at least 1"));
    }

    #[test]
    fn incomplete_slots_of_kind_follows_the_slots_current_kind() {
        // The same re-registration consequence
        // `invocations_of_kind_follows_the_slots_current_kind` pins for the
        // counter, pinned for the flag — because the two are read together
        // and a divergence between them would be invisible.
        //
        // This is also the one direction that matters for the gate: promoting
        // a bypassed stub to `Bridge` moves BOTH the count and the blindness
        // off the stub verdict, which is correct (the stub was fixed). The
        // reverse — demoting a marked `Bridge` to `SyntheticStub` — makes the
        // stub verdict unmeasurable, which is exactly when a gate must go red.
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("p/P", "m", "()I", dummy_native);
        });
        let id = registry.resolve_id("p/P", "m", "()I").expect("registered");
        registry.mark_invocations_incomplete(id);
        assert_eq!(registry.incomplete_slots_of_kind(NativeKind::Bridge), 1);
        assert_eq!(
            registry.incomplete_slots_of_kind(NativeKind::SyntheticStub),
            0
        );
        assert!(registry
            .invocations_of_kind_checked(NativeKind::SyntheticStub)
            .is_clean());

        // Demote the same triple to a stub. The bit is sticky by design, so
        // the blindness travels with the slot.
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("p/P", "m", "()I", dummy_native_2);
        });
        assert_eq!(
            registry.resolve_id("p/P", "m", "()I"),
            Some(id),
            "re-registration must update the slot in place"
        );
        assert_eq!(registry.incomplete_slots_of_kind(NativeKind::Bridge), 0);
        assert_eq!(
            registry.incomplete_slots_of_kind(NativeKind::SyntheticStub),
            1
        );
        assert!(
            !registry
                .invocations_of_kind_checked(NativeKind::SyntheticStub)
                .is_clean(),
            "a bypassed slot that becomes a stub must turn the gate red, not quiet"
        );
    }

    #[test]
    fn the_bais_hook_is_absent_until_installed() {
        // The BAIS observer is a process-wide `OnceLock` and this crate never
        // installs one, so `native-io`'s dispatch sites are inert here — the
        // property that makes adding them a no-op for every consumer until
        // the nominated `http_url_connection.rs` hook lands. Asserted through
        // the public surface rather than by reading the static, so it stays
        // true if the storage changes.
        //
        // Deliberately not a behavioural test of a hook: installing one would
        // be irreversible for every other test in this binary. The
        // behavioural coverage lives in `native-io`, thread-armed for exactly
        // that reason.
        assert!(
            BAIS_EVENT_HOOK.get().is_none(),
            "no hook may be installed from within native-api's own tests"
        );
    }

    #[test]
    fn census_reports_provenance_and_zeroes_superseded_rows() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("s/S", "m", "()I", dummy_native);
        });
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("s/S", "m", "()I", dummy_native_2);
        });
        let id = registry.resolve_id("s/S", "m", "()I").expect("registered");
        registry.record_invocation(id);
        registry.record_invocation(id);
        registry.record_invocation(id);

        let census = registry.census();
        assert_eq!(census.len(), 2, "one row per registration, not per slot");

        // Row 0: the superseded stub. It knows it was first (`overwrote: None`)
        // and reports zero invocations — the count belongs to whoever owns the
        // slot now, and double-attributing it would inflate the CI gate's
        // synthetic-stub total.
        assert_eq!(census[0].class, "s/S");
        assert_eq!(census[0].name, "m");
        assert_eq!(census[0].descriptor, "()I");
        assert_eq!(census[0].kind, NativeKind::SyntheticStub);
        assert_eq!(census[0].overwrote, None);
        assert_eq!(census[0].invocations, 0);
        // ... and it owns no slot, which is the fact that makes the zero above
        // mean "cannot be dispatched" rather than "was not dispatched yet".
        // Adjudicating this row's kind decides nothing.
        assert!(
            !census[0].owns_slot,
            "a superseded registration must not claim to own its slot"
        );

        // Row 1: the current owner. `overwrote` is the history that was
        // previously unrecoverable — it is read before the in-place slot update.
        assert_eq!(census[1].kind, NativeKind::Bridge);
        assert_eq!(census[1].overwrote, Some(NativeKind::SyntheticStub));
        assert_eq!(census[1].invocations, 3);
        assert!(
            census[1].owns_slot,
            "the surviving registration must own its slot"
        );

        for row in &census {
            let site = row.registered_by.as_deref().expect("provenance recorded");
            assert!(
                site.contains("registry.rs:"),
                "expected a file:line site, got {site}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Capability gate (registration + dispatch chokepoints)
    // -----------------------------------------------------------------------

    use crate::capability::{
        Capability, CapabilityKind, CapabilityMode, CapabilitySet, Scope, VmId,
    };

    #[test]
    fn no_capability_policy_means_no_gate_at_all() {
        // The default must be byte-for-byte today's behaviour: registration
        // accepted, no classification recorded, dispatch check trivially Ok.
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/ProcessBuilder", "start", "()V", dummy_native);
        let id = registry
            .resolve_id("java/lang/ProcessBuilder", "start", "()V")
            .expect("registered");
        assert!(registry.capabilities().is_none());
        assert_eq!(registry.capability_of_id(id), None);
        assert!(registry.check_dispatch_capability(id).is_ok());
    }

    #[test]
    fn permissive_policy_classifies_but_never_refuses() {
        let mut registry = NativeMethodRegistry::new();
        let caps = std::sync::Arc::new(CapabilitySet::new(
            VmId::from_raw(0xCA9A_0001),
            CapabilityMode::Permissive,
        ));
        registry.set_capabilities(std::sync::Arc::clone(&caps));

        registry.register("java/lang/ProcessBuilder", "start", "()V", dummy_native);
        registry.register("java/util/HashMap", "put", "()V", dummy_native_2);

        let spawn = registry
            .resolve_id("java/lang/ProcessBuilder", "start", "()V")
            .expect("permissive must accept the registration");
        let benign = registry
            .resolve_id("java/util/HashMap", "put", "()V")
            .expect("registered");

        assert_eq!(
            registry.capability_of_id(spawn),
            Some(CapabilityKind::ProcessSpawn)
        );
        assert_eq!(registry.capability_of_id(benign), None);
        assert!(registry.check_dispatch_capability(spawn).is_ok());
        assert!(registry.check_dispatch_capability(benign).is_ok());

        // Both the registration and the dispatch are in the audit log, so a
        // deployment can see what it would have to grant.
        let report = caps.audit_report();
        assert!(
            report.uses.iter().any(|u| {
                u.capability.kind() == CapabilityKind::NativeRegister
                    && u.capability.scope().to_string() == "java/lang/ProcessBuilder.start"
            }),
            "registration must be recorded:\n{report}"
        );
        assert!(
            report
                .uses
                .iter()
                .any(|u| u.capability.kind() == CapabilityKind::ProcessSpawn),
            "dispatch must be recorded:\n{report}"
        );
    }

    #[test]
    fn enforce_refuses_ungranted_registration_without_inserting() {
        let mut registry = NativeMethodRegistry::new();
        let mut set = CapabilitySet::new(VmId::from_raw(0xCA9A_0002), CapabilityMode::Enforce);
        // Grant registration of the HashMap native only.
        set.grant(Capability::NativeRegister(Scope::name(
            "java/util/HashMap.*",
        )));
        registry.set_capabilities(std::sync::Arc::new(set));

        let generation_before = registry.generation();
        registry.register("java/lang/ProcessBuilder", "start", "()V", dummy_native);
        assert!(
            registry
                .find("java/lang/ProcessBuilder", "start", "()V")
                .is_none(),
            "a refused registration must not be reachable"
        );
        assert_eq!(
            registry.generation(),
            generation_before,
            "a refused registration must not move the registry generation"
        );
        assert!(
            registry.dump_registrations().is_empty(),
            "a refused registration must not appear in the census"
        );

        // The granted one still lands.
        registry.register("java/util/HashMap", "put", "()V", dummy_native_2);
        assert!(registry.find("java/util/HashMap", "put", "()V").is_some());
    }

    #[test]
    fn enforce_gates_dispatch_of_a_classified_native() {
        let mut registry = NativeMethodRegistry::new();
        let mut set = CapabilitySet::new(VmId::from_raw(0xCA9A_0003), CapabilityMode::Enforce);
        // Registration of everything is allowed; the *use* of a spawn native
        // is not. This is the layering the design intends: install natives
        // freely, gate what they do.
        set.grant(Capability::NativeRegister(Scope::Any));
        set.grant(Capability::LibraryLoad(Scope::Any));
        registry.set_capabilities(std::sync::Arc::new(set));

        registry.register("java/lang/ProcessBuilder", "start", "()V", dummy_native);
        registry.register("java/lang/System", "loadLibrary", "()V", dummy_native_2);

        let spawn = registry
            .resolve_id("java/lang/ProcessBuilder", "start", "()V")
            .expect("registered");
        let load = registry
            .resolve_id("java/lang/System", "loadLibrary", "()V")
            .expect("registered");

        let denied = registry
            .check_dispatch_capability(spawn)
            .expect_err("process-spawn is not granted");
        assert_eq!(denied.capability, CapabilityKind::ProcessSpawn);
        assert_eq!(denied.vm, VmId::from_raw(0xCA9A_0003));
        // `library-load:*` IS granted, so that native dispatches.
        assert!(registry.check_dispatch_capability(load).is_ok());
    }

    #[test]
    fn reregistration_updates_the_slots_capability_classification() {
        // Re-registration rewrites the slot in place (that is what keeps an
        // already-issued NativeMethodId valid), so the classification must
        // track the triple that currently owns the slot — including dropping
        // it when the new owner is not capability-relevant.
        let mut registry = NativeMethodRegistry::new();
        let caps = std::sync::Arc::new(CapabilitySet::new(
            VmId::from_raw(0xCA9A_0004),
            CapabilityMode::Permissive,
        ));
        registry.set_capabilities(caps);

        registry.register("java/lang/Runtime", "loadLibrary0", "()V", dummy_native);
        let id = registry
            .resolve_id("java/lang/Runtime", "loadLibrary0", "()V")
            .expect("registered");
        assert_eq!(
            registry.capability_of_id(id),
            Some(CapabilityKind::LibraryLoad)
        );

        // Same triple, new callback: the slot is reused and stays classified.
        registry.register("java/lang/Runtime", "loadLibrary0", "()V", dummy_native_2);
        assert_eq!(
            registry.resolve_id("java/lang/Runtime", "loadLibrary0", "()V"),
            Some(id),
            "re-registration must reuse the slot"
        );
        assert_eq!(
            registry.capability_of_id(id),
            Some(CapabilityKind::LibraryLoad)
        );
    }

    #[test]
    fn two_registries_do_not_share_a_capability_policy() {
        // The whole point of putting the policy in a field: two VMs in one
        // process must not see each other's decisions.
        let mut strict = NativeMethodRegistry::new();
        let mut lax = NativeMethodRegistry::new();
        strict.set_capabilities(std::sync::Arc::new(CapabilitySet::new(
            VmId::from_raw(0xCA9A_0005),
            CapabilityMode::Enforce,
        )));
        lax.set_capabilities(std::sync::Arc::new(CapabilitySet::new(
            VmId::from_raw(0xCA9A_0006),
            CapabilityMode::Permissive,
        )));

        strict.register("java/lang/ProcessBuilder", "start", "()V", dummy_native);
        lax.register("java/lang/ProcessBuilder", "start", "()V", dummy_native);

        assert!(
            strict
                .find("java/lang/ProcessBuilder", "start", "()V")
                .is_none(),
            "the enforcing registry refuses"
        );
        assert!(
            lax.find("java/lang/ProcessBuilder", "start", "()V")
                .is_some(),
            "the permissive registry is unaffected by the other VM's policy"
        );
    }

    // ----- G56-1: the slot-indexed raw accessors ---------------------------
    //
    // `MockNativeContext` overrides neither `get_field_typed` nor
    // `get_field_raw`/`set_field_raw`, so every test below exercises the trait
    // DEFAULTS — which is the code this lane added and the code every mock in
    // the tree will inherit.

    fn fresh_object(ctx: &mut MockNativeContext) -> ObjectRef {
        match ctx.new_object("java/lang/Object") {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("mock new_object must hand back an object, got {other:?}"),
        }
    }

    /// The sentinel has to be a byte no class file can produce.
    ///
    /// JVMS §4.3.2: a field descriptor begins with one of `B C D F I J S Z L
    /// [`. `RAW_SLOT_DESCRIPTOR` must be outside that set, or `get_field_raw`
    /// would decode the slot as whichever type it collided with — silently, and
    /// only for slots whose real descriptor differs.
    #[test]
    fn the_raw_slot_descriptor_is_not_a_jvm_field_descriptor() {
        for d in b"BCDFIJSZL[" {
            assert_ne!(
                RAW_SLOT_DESCRIPTOR, *d,
                "the raw sentinel collided with the real descriptor '{}'",
                *d as char
            );
        }
    }

    /// A raw read hands back the tag that is stored, not a decoded one.
    ///
    /// The distinction this accessor exists for is `Int(0)` vs `Object(None)`
    /// at a slot the class declares `L`: the first is a never-written slot
    /// (`gen_heap::read_slot`'s R-niche rule), the second is an explicit null.
    /// Every other slot-indexed reader on the trait collapses them.
    #[test]
    fn get_field_raw_hands_back_the_stored_tag_verbatim() {
        let mut ctx = MockNativeContext::new();
        let obj = fresh_object(&mut ctx);

        for (slot, stored) in [
            Value::Int(0),
            Value::Int(-7),
            Value::Long(1 << 40),
            Value::Float(0.5),
            Value::Double(-2.5),
            Value::Object(None),
            Value::Uninitialized,
        ]
        .into_iter()
        .enumerate()
        {
            ctx.set_field(obj, slot, stored);
            assert_eq!(
                ctx.get_field_raw(obj, slot),
                stored,
                "slot {slot} must read back byte-identically"
            );
        }

        // The falsifier: a reader that always answered null, or always
        // answered the descriptor default, would pass every assertion above
        // for exactly one of these two and fail for the other.
        ctx.set_field(obj, 20, Value::Int(0));
        ctx.set_field(obj, 21, Value::Object(None));
        assert_ne!(
            ctx.get_field_raw(obj, 20),
            ctx.get_field_raw(obj, 21),
            "a raw read must keep `Int(0)` and `Object(None)` distinguishable -- \
             that is the entire capability being added"
        );
    }

    /// A live reference survives the round trip.
    ///
    /// This is the case that makes a verbatim `Object.clone()` possible and the
    /// one where the coercing pair can do real damage: an `Object(Some(_))`
    /// sitting in a slot the class declares primitive is truncated to
    /// `Int(ptr as i32)` by `coerce_field_value_for_slot`, a stale address no
    /// collector will remap (G52-1 §1.6).
    #[test]
    fn the_raw_pair_round_trips_a_live_reference() {
        let mut ctx = MockNativeContext::new();
        let src = fresh_object(&mut ctx);
        let dst = fresh_object(&mut ctx);
        let payload = fresh_object(&mut ctx);

        ctx.set_field(src, 3, Value::Object(Some(payload)));
        let copied = ctx.get_field_raw(src, 3);
        ctx.set_field_raw(dst, 3, copied);

        assert_eq!(
            ctx.get_field_raw(dst, 3),
            Value::Object(Some(payload)),
            "the copy must hold the SAME reference, not a truncated address"
        );
    }

    /// The store half is a real store, and it is the one a copy loop uses.
    ///
    /// The default delegates to `set_field`; this pins that the delegation
    /// exists and lands in the same slot the raw reader reads. It does NOT
    /// claim the production `NativeContextImpl` stores raw — see the method's
    /// own doc comment, which says the opposite in as many words.
    #[test]
    fn set_field_raw_writes_the_slot_the_raw_reader_reads() {
        let mut ctx = MockNativeContext::new();
        let obj = fresh_object(&mut ctx);

        ctx.set_field_raw(obj, 5, Value::Long(0x5EED));
        assert_eq!(ctx.get_field_raw(obj, 5), Value::Long(0x5EED));
        assert_eq!(
            ctx.get_field(obj, 5),
            Value::Long(0x5EED),
            "the raw and ordinary readers must agree about WHICH slot was written"
        );
    }

    /// A two-line verbatim copy, which is the whole point of the pair.
    ///
    /// `Object.clone()` is specified as a verbatim field copy. Before these
    /// accessors existed there was no slot-indexed way to express one from a
    /// native: `get_field`/`set_field` resolve and coerce, and the only raw
    /// pair was name-keyed (G52-1 §1.4). This is the loop the follow-up in
    /// `native-builtins/src/lib.rs` is expected to become.
    #[test]
    fn the_raw_pair_expresses_a_verbatim_field_copy() {
        let mut ctx = MockNativeContext::new();
        let src = fresh_object(&mut ctx);
        let dst = fresh_object(&mut ctx);

        // A receiver whose slots hold three tags the descriptor pair would
        // rewrite: a never-written reference (raw zero), an explicit null, and
        // a long.
        let planted = [Value::Int(0), Value::Object(None), Value::Long(-1)];
        for (i, v) in planted.iter().enumerate() {
            ctx.set_field(src, i, *v);
        }

        for i in 0..planted.len() {
            let v = ctx.get_field_raw(src, i);
            ctx.set_field_raw(dst, i, v);
        }

        for (i, v) in planted.iter().enumerate() {
            assert_eq!(
                ctx.get_field_raw(dst, i),
                *v,
                "slot {i} of the copy diverged from the original"
            );
        }
    }
}
