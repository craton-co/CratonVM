// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Utility functions: class initialization, preparation, descriptor helpers,
//! and the ClassStoreHierarchy adapter for the bytecode verifier.
//!
//! # `<clinit>` failure policy (JVMS §5.5)
//!
//! When a class's `<clinit>` throws, the **default** behavior of this module is
//! the JVMS-conformant one: the class is transitioned to
//! [`ClassState::InitializationError`] and the failure is propagated to the
//! caller (an `Error` subclass propagates as-is; any other throwable is wrapped
//! in `ExceptionInInitializerError`). **No** static state is fabricated. This is
//! correct for an open-source JVM and avoids masking real initialization bugs in
//! user code.
//!
//! For the framework "app gauntlet" (Quarkus / JBoss / Spring / WildFly /
//! SLF4J-logback / ICU, …) some real `<clinit>` paths currently fail because of
//! capability gaps (resource loading, module loading, logging-backend init).
//! Setting **`CRATONVM_LENIENT_CLINIT=1`** restores the legacy lenient mode: for
//! an allowlisted set of framework/JDK packages and a list of "recoverable"
//! exception types, the failure is *swallowed*, the class is marked
//! `Initialized` anyway, and [`post_clinit_fixup`] backfills the load-bearing
//! statics that downstream framework code reads (these app-specific synthetic
//! backfills are **only** reachable under this gate). Every swallow under the
//! lenient gate emits a one-line `tracing::warn!` so the divergence is visible.
//!
//! This inverts the historical default (which was lenient, with
//! `CRATONVM_STRICT_SWALLOWS=1` opting *into* the correct behavior). The strict
//! escalation gate is still honoured where present, but it is now a no-op for
//! the swallow path because swallowing no longer happens by default.

use crate::classloading::{find_field_recursive, Class, ClassId, ClassState, ClassStore};
use crate::error::{LinkageError, MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::JvmThread;
use crate::types::Value;
use cratonvm_types::ArrayElementType;
use cratonvm_types::ObjectRef;
use std::sync::Arc;

use super::SharedVm;

/// `CRATONVM_LENIENT_CLINIT=1` re-enables the legacy lenient `<clinit>`-failure
/// handling: swallow the failure for an allowlisted framework/JDK package set,
/// mark the class `Initialized`, and run [`post_clinit_fixup`]'s app-specific
/// synthetic backfills. **Default OFF** — when unset the JVMS-correct path runs
/// (mark `InitializationError` + throw; no synthetic state). See the module doc
/// comment. Cached once for the process lifetime; only the literal `"1"` enables
/// it (mirroring the `CRATONVM_STRICT_SWALLOWS` convention).
#[inline]
fn lenient_clinit() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_LENIENT_CLINIT") {
            Ok(v) => v == "1",
            Err(_) => false,
        },
    )
}

/// Whether a swallowed `<clinit>` failure for `class_name` has a *documented,
/// genuinely-needed* recovery path that leaves the class usable.
///
/// Historically the lenient `<clinit>` swallow used a broad prefix allowlist
/// (`java/`, `jdk/`, `sun/`, `javax/`, `org/jboss/`, `io/quarkus/`,
/// `org/wildfly/`, `io/smallrye/`, `org/springframework/boot/loader/`,
/// `org/slf4j/impl/`, `ch/qos/logback/`, `com/sun/`) crossed with a long
/// list of "non-critical" exception types. That swallowed — and silently
/// marked `Initialized` — *any* framework class whose `<clinit>` failed,
/// even when nothing repairs the half-initialized statics. The result was a
/// class that linked but whose load-bearing statics were left null, surfacing
/// later as a bare NPE far from the real cause and masking genuine init bugs.
///
/// This narrows the swallow to **exactly** the cases that are recovered:
///
///   1. Every class with a [`post_clinit_fixup`] match arm — the fixup
///      backfills the statics that downstream framework code reads, so the
///      class is genuinely usable after the swallow. Each entry below is
///      kept in lock-step with a `post_clinit_fixup` arm; if you add a
///      fixup arm, add it here too (and vice-versa).
///   2. The SLF4J/logback binder packages (`org/slf4j/impl/`,
///      `ch/qos/logback/`) — these have **no** `post_clinit_fixup` arm, but
///      `register_slf4j_binder_stubs_pub` (vm_init.rs) provides native
///      singletons for `getSingleton()`/`getLoggerFactory()`/MDC/Marker
///      binders, so SLF4J's `LoggerFactory.bind()` never reads the
///      half-initialized `SINGLETON` static. Documented framework-compat
///      case; retained explicitly.
///
/// Any other framework/JDK class is **no longer** swallowed even under
/// `CRATONVM_LENIENT_CLINIT=1`: the failure propagates as
/// `ExceptionInInitializerError` (JVMS §5.5) so real init bugs surface at
/// their true origin instead of being hidden behind a stamped-`Initialized`
/// class with null statics.
fn clinit_swallow_has_recovery(class_name: &str) -> bool {
    // NOTE (real-cdi-bean-container Step 3): Spring's `ApplicationStartup`
    // startup-metrics subsystem runs its REAL bytecode and is intentionally NOT
    // in this allowlist — its `<clinit>` must complete on its own and any failure
    // surface per JVMS §5.5, never be swallowed + backfilled. The legacy no-op
    // shim and its `post_clinit_fixup` backfill have been removed.
    // (1) Classes with an explicit `post_clinit_fixup` recovery arm.
    let has_fixup_arm = matches!(
        class_name,
        "java/util/logging/LogManager"
            | "jdk/internal/icu/text/NormalizerBase$NFCModeImpl"
            | "jdk/internal/icu/text/NormalizerBase$NFDModeImpl"
            | "jdk/internal/icu/text/NormalizerBase$NFKCModeImpl"
            | "jdk/internal/icu/text/NormalizerBase$NFKDModeImpl"
            | "jdk/internal/icu/text/NormalizerBase$NFKC32ModeImpl"
            | "java/lang/invoke/VarHandleInts$Array"
            | "java/lang/invoke/VarHandleLongs$Array"
            | "java/lang/invoke/VarHandleShorts$Array"
            | "java/lang/invoke/VarHandleBytes$Array"
            | "java/lang/invoke/VarHandleBooleans$Array"
            | "java/lang/invoke/VarHandleChars$Array"
            | "java/lang/invoke/VarHandleFloats$Array"
            | "java/lang/invoke/VarHandleDoubles$Array"
            | "java/lang/invoke/VarHandleReferences$Array"
            | "io/quarkus/bootstrap/logging/InitialConfigurator"
            | "org/jboss/modules/DefaultBootModuleLoaderHolder"
            | "java/math/BigInteger"
            | "java/util/concurrent/TimeUnit"
            | "java/nio/file/attribute/PosixFilePermission"
            | "java/math/BigDecimal"
            | "org/jboss/msc/service/ServiceContainerImpl"
            | "org/wildfly/security/auth/server/_private/ElytronMessages"
            | "org/jboss/msc/service/ServiceLogger"
    );
    if has_fixup_arm {
        return true;
    }
    // (2) SLF4J/logback binder packages — recovered by native binder stubs,
    // not by `post_clinit_fixup`. See doc comment above.
    class_name.starts_with("org/slf4j/impl/") || class_name.starts_with("ch/qos/logback/")
}

/// Lazily allocate and cache the canonical `System.in` `FileInputStream` (stdin fd 0).
///
/// Surefire's `LegacyMasterProcessChannelProcessorFactory` calls
/// `Channels.newBufferedChannel(System.in)` during `ForkedBooter.setupBooter`
/// **before** `System.initPhase1` has run far enough for the static field to
/// be populated. `GETSTATIC System.in` must therefore never observe null.
///
/// BC-shim BCJ-1 (2026-05-27): set the `fd` `FileDescriptor` object on the
/// synthetic FIS so the `native_fis_read` / `native_fis_read_bytes` natives
/// can recover fd 0 via the standard `fis_fd_object` path. The earlier
/// scheme of stuffing `fd+1` into instance slot 1 was a leftover from the
/// pre-real-JDK layout: in the JDK 25 `FileInputStream` field layout slot 1
/// is `path: String`, so `get_field(this, 1)` no longer yields the
/// `Value::Int(1)` we wrote (type-mismatched writes to a reference slot get
/// reinterpreted as `Value::Object(None)` at read time), `fis_get_fd`
/// returns `None`, and `System.in.read()` always returned -1. This breaks
/// any caller that reads stdin through `System.in` — including Gradle's
/// `GradleWorkerMain` which pulls a binary protocol from stdin before
/// running tests (reproduced 2026-05-27 against bc-java :core:test;
/// `NegativeArraySizeException` from `anewarray URL[-1]`).
pub fn ensure_system_stdin_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(r) = *shared.system_in.read() {
        return Ok(r);
    }
    let mut guard = shared.system_in.write();
    if let Some(r) = *guard {
        return Ok(r);
    }
    let fis_class_id = shared.load_class_concurrent("java/io/FileInputStream")?;
    ensure_class_initialized_shared(shared, thread, fis_class_id)?;
    let num_fields = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(fis_class_id)
            .map(|c| c.num_total_fields.max(2))
            .unwrap_or(2)
    };
    let in_obj = shared.mem.heap.alloc_object(fis_class_id, num_fields);
    // gcstress residual face fix — pin `in_obj` across the FileDescriptor
    // class-load/clinit/alloc window below. Those calls allocate, and under
    // allocation pressure a MOVING young GC can run inside the window; this
    // Rust local is invisible to `collect_roots` (the A1 "Rust local across
    // allocating calls" family), so the later `set_field(in_obj, ..)` wrote
    // through the STALE pre-move address (observed as the bootstrap
    // "set_field: out-of-bounds field write dropped" on an all-zero header
    // under CRATONVM_DBG_GC_STRESS, leaving System.in without its fd and the
    // stale ref cached in `shared.system_in`). `native_pin_roots` is scanned
    // as a root AND remapped in place by every collection, so re-reading the
    // pin after the window yields the object's current address.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(in_obj);

    // Build a real `FileDescriptor` carrying fd=0 and pin it to the FIS's
    // `fd` slot. The `native_fis_read*` path resolves the fd via
    // `fis_fd_object` -> `FileDescriptor.fd`, so the descriptor's int `fd`
    // field carries the kernel handle.
    //
    // The fallible calls run through an explicit match (not `?`) so the pin
    // pushed above is truncated on the error path too — a leaked pin entry
    // would over-retain and unbalance `native_pin_roots` forever.
    let fd_setup: Result<(ClassId, usize), MethodCallFailed> = (|| {
        let fd_class_id = shared.load_class_concurrent("java/io/FileDescriptor")?;
        ensure_class_initialized_shared(shared, thread, fd_class_id)?;
        let fd_num_fields = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(fd_class_id)
                .map(|c| c.num_total_fields.max(2))
                .unwrap_or(2)
        };
        Ok((fd_class_id, fd_num_fields))
    })();
    let (fd_class_id, fd_num_fields) = match fd_setup {
        Ok(v) => v,
        Err(e) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e);
        }
    };
    let fd_obj = shared.mem.heap.alloc_object(fd_class_id, fd_num_fields);
    // End of the allocating window: re-read the (possibly remapped) pin and
    // release it. `fd_obj` was the last allocation, so it cannot go stale
    // before the writes below.
    let in_obj = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    // Find the `fd` / `handle` field indices by name so we don't depend on
    // a hardcoded slot order. `fd` is an int (kernel fd, used on POSIX and
    // mirrored on Windows); `handle` is a long (Windows HANDLE), match the
    // real JDK layout. Setting both keeps `fis_get_fd` happy on either
    // platform.
    let (fd_field_idx, handle_field_idx) = {
        let cm = shared.classes.class_manager.read();
        let fd_idx = find_field_recursive(fd_class_id, "fd", &cm.class_store).map(|(i, _, _)| i);
        let h_idx = find_field_recursive(fd_class_id, "handle", &cm.class_store).map(|(i, _, _)| i);
        (fd_idx, h_idx)
    };
    if let Some(idx) = fd_field_idx {
        shared.mem.heap.set_field(fd_obj, idx, Value::Int(0));
    }
    if let Some(idx) = handle_field_idx {
        shared.mem.heap.set_field(fd_obj, idx, Value::Long(0));
    }

    // Pin the FileDescriptor object on the FIS's `fd` slot.
    let fis_fd_slot = {
        let cm = shared.classes.class_manager.read();
        find_field_recursive(fis_class_id, "fd", &cm.class_store).map(|(i, _, _)| i)
    };
    if let Some(idx) = fis_fd_slot {
        shared
            .mem
            .heap
            .set_field(in_obj, idx, Value::Object(Some(fd_obj)));
    } else {
        // JDK-ONLY-LAYOUT: safe (was `breaks-under-strict`, wave 1).
        //
        // Slot 1 of a REAL `java/io/FileInputStream` is `path:String`, and
        // writing `Int(1)` there would be a silent wrong-field write. It cannot
        // happen: this arm is reached only when `find_field_recursive(.., "fd")`
        // answers `None`, i.e. the receiver's class declares no `fd` ANYWHERE on
        // its superclass chain — which is the fabricated-layout predicate, asked
        // by NAME, and the shape every fix in this family converged on. A real
        // `java.io.FileInputStream` always declares `fd`.
        //
        // Slot 1 on that fabricated layout is the legacy `fd + 1` marker
        // `native-io`'s `fis_get_fd` reads last ("`System.in`: slot 1 holds
        // fd+1"), so `Int(1)` means fd 0, stdin — which is what this call site
        // is building.
        //
        // What was missing was not a guard but a WITNESS: the arm had no way to
        // announce itself, so "cannot happen" was an argument rather than an
        // observation. Under `CompatibilityMode::JdkOnly` a fabricated
        // `FileInputStream` is a policy violation in its own right, and the
        // warning names it at the one place that can see it.
        if shared.compatibility_mode().is_jdk_only() {
            tracing::warn!(
                "jdk-only: System.in built on a fabricated java/io/FileInputStream \
                 (no `fd` field to resolve); writing the legacy slot-1 fd marker"
            );
        }
        shared.mem.heap.set_field(in_obj, 1, Value::Int(1));
    }

    *guard = Some(in_obj);
    Ok(in_obj)
}

// ---------------------------------------------------------------------------
// Free functions: class initialization
// ---------------------------------------------------------------------------

thread_local! {
    /// Nesting depth of `<clinit>` frames currently executing on this
    /// (OS) thread, including nested/re-entrant `<clinit>` calls one
    /// static initializer transitively triggers. See `in_clinit_shared`.
    static CLINIT_NESTING_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// True while this thread is executing inside some class's `<clinit>`.
///
/// Reflective code paths (e.g. building `java.lang.reflect.Method`
/// mirrors) must consult this before forcing an UNRELATED class's full
/// initialization as a side effect of merely exposing its return/parameter
/// type. JVMS §5.5 triggers initialization only via `new`/`getstatic`/
/// `putstatic`/`invokestatic` on that exact class, never as a side effect
/// of reflection over a *different*, currently-initializing class. Forcing
/// it anyway lets the newly-initialized class observe the in-progress
/// class's static fields at their pre-assignment default (usually `null`)
/// instead of failing to resolve at all -- see
/// `docs/known-issues/springboot/netty-compositebytebuf-clinit-reads-unpooled-empty-buffer-null-20260731.md`
/// for the concrete repro (Netty's `Unpooled.<clinit>` -> `UnpooledByteBufAllocator`
/// superclass init -> `ResourceLeakDetector.addExclusions` ->
/// `Class.getDeclaredMethods()` on `AbstractByteBufAllocator`, whose declared
/// `compositeBuffer()` return type `CompositeByteBuf` was being force-initialized
/// mid-way through `Unpooled.<clinit>`, before `EMPTY_BUFFER` was assigned).
pub fn in_clinit_shared() -> bool {
    CLINIT_NESTING_DEPTH.with(|d| d.get() > 0)
}

/// RAII depth counter paired with `in_clinit_shared`. Increment on
/// `<clinit>` entry, decrement on every exit (including panics unwinding
/// through the guarded scope).
struct ClinitDepthGuard;

impl ClinitDepthGuard {
    fn enter() -> Self {
        CLINIT_NESTING_DEPTH.with(|d| d.set(d.get() + 1));
        ClinitDepthGuard
    }
}

impl Drop for ClinitDepthGuard {
    fn drop(&mut self) {
        CLINIT_NESTING_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Ensure a class is fully initialized (JVM spec В§5.5).
///
/// Handles three cases:
/// 1. Already `Initialized` в†’ return immediately.
/// 2. `Initializing` by the **same** thread в†’ return immediately (re-entrancy).
/// 3. `Initializing` by a **different** thread в†’ block until it finishes,
///    then check the final state.
/// 4. `InitializationError` в†’ return `NoClassDefFoundError`.
/// 5. Any other state в†’ run the full initialization sequence.
/// `CRATONVM_DBG_CLINIT_ORDER=1` — name every class this thread claims for
/// `<clinit>`, in order, nested by depth, with the frame that triggered it.
///
/// The sibling `CRATONVM_DBG_CLINIT_FAIL` reports a Rust backtrace for an
/// initializer that *threw*. This reports the ORDER, which is the different
/// question — and the only one that can be answered for a JDK circular-init
/// cycle such as `java/lang/constant/ConstantDescs` <->
/// `jdk/internal/constant/PrimitiveClassDescImpl`, whose correctness is
/// entirely "which of the two is entered first, and at which statement".
/// Diffing this between two arms of a flag matrix is what identified the
/// `CRATONVM_BG_COMPILE=0` `<clinit>` first-call-compile defect: the failing
/// arm reached `PrimitiveClassDescImpl` after 16 nested inits instead of 36,
/// and its trigger frame never advanced past the caller — i.e. the enclosing
/// `<clinit>` was not executing its own statements at all.
///
/// Off by default; one `OnceLock<bool>` read per class initialization, which is
/// not a hot path.
fn clinit_order_trace(shared: &SharedVm, thread: &JvmThread, class_id: ClassId, phase: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CLINIT_ORDER").is_some()
    }) {
        return;
    }
    use std::cell::Cell;
    thread_local! { static DEPTH: Cell<usize> = const { Cell::new(0) }; }
    let name = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<cid {class_id}>"));
    let d = DEPTH.with(|d| d.get());
    if phase == "claim" {
        // The TRIGGERING site, not just the class: for a JDK circular-init cycle
        // the question is always "which statement of the enclosing <clinit>
        // reached this", and a bare class list cannot answer it.
        let top: Vec<String> = thread
            .frames
            .iter()
            .rev()
            .take(3)
            .map(|f| format!("{}.{}@{}", f.class_name(), f.method_name(), f.last_instr_pc))
            .collect();
        let from = format!("depth={} [{}]", thread.frames.len(), top.join(" <- "));
        eprintln!(
            "[clinit-order] {:width$}> {name}   from {from}",
            "",
            width = d * 2
        );
        DEPTH.with(|x| x.set(d + 1));
    } else {
        let nd = d.saturating_sub(1);
        DEPTH.with(|x| x.set(nd));
        eprintln!(
            "[clinit-order] {:width$}< {name} {phase}",
            "",
            width = nd * 2
        );
    }
}

pub fn ensure_class_initialized_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    // Round 5 audit fix (HIGH): AtomicU8 fast path. After warmup the
    // overwhelming majority of `ensure_class_initialized_shared` calls
    // are for classes that are already fully initialized — the previous
    // implementation paid for a `class_manager.read()` RwLock + a
    // `get_class` + a `ClassState` match on every single call (interpreter
    // dispatch, JIT entry, reflection). A single relaxed atomic load
    // returns immediately on the steady-state hot path.
    //
    // Round-9 vm CRIT-1 / classloading CRIT-1 fix (audits
    // `round9-vm.md` and `round9-classloading-reader.md`): the previous
    // "fast path" called `class_init_state_handle(class_id)` which
    // ALWAYS probed the side-table RwLock first, and then
    // `set_class_init_state` auto-populated that side-table for every
    // real class on init completion — so the per-`Class` embedded
    // `init_state: Arc<AtomicU8>` added by round-8 was dead code on
    // the steady-state hot path. The fix probes the embedded atomic
    // DIRECTLY: a single `class_manager.read()` (held only long enough
    // to obtain `&Class`) → `class.init_state.load(Acquire)`. No
    // `init_states` lock acquisition, no FxHashMap probe, no Arc
    // clone on the fast path.
    //
    // For a CLASS_INIT_INITIALIZED hit (the overwhelming common case),
    // this collapses to: one parking_lot read-lock acquire/release on
    // the class manager + one `Arc<AtomicU8>::load(Acquire)`. The
    // load needs no lock because the embedded atomic lives inside the
    // `Class` we just borrowed.
    if crate::runtime::env_cache::modstatic_dbg() {
        let nm = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string());
        if nm.as_deref() == Some("org/jboss/modules/Module") {
            let fast = is_class_initialized_via_manager(shared, class_id);
            let st = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.state);
            eprintln!("MODSTATIC: ensure_init(Module) fast_initialized={fast} state={st:?}");
        }
    }
    if is_class_initialized_via_manager(shared, class_id) {
        return Ok(());
    }

    // Known JDK <clinit> order cycle (slow path only). Cold-starting
    // `jdk/internal/constant/PrimitiveClassDescImpl` FIRST is fatal:
    // its `CD_int = new PrimitiveClassDescImpl("I")` ctor reads
    // `ConstantDescs.BSM_PRIMITIVE_CLASS`, which nests
    // `ConstantDescs.<clinit>`; that clinit's own
    // `CD_int = PrimitiveClassDescImpl.CD_int` (ConstantDescs.java:249)
    // re-enters the in-progress PrimitiveClassDescImpl and reads null, and
    // `ofConstantBootstrap(..., CD_int)` then dies in requireNonNull → NPE
    // → ExceptionInInitializerError → both classes are poisoned
    // (NoClassDefFoundError for the rest of the process). This is the
    // JDK's own circularity: stock HotSpot only survives because CDS
    // pre-initializes ConstantDescs (`java -Xshare:off` reproduces the
    // identical EIIE on HotSpot 25). Surfaced by Spring 7's JDK-24+
    // ClassFile MetadataReader (Utf8EntryImpl.methodTypeSymbol →
    // MethodTypeDescImpl.ofDescriptor) during @Configuration parsing.
    //
    // Fix: before CLAIMING PrimitiveClassDescImpl, fully initialize
    // ConstantDescs. Its clinit nests PrimitiveClassDescImpl in the
    // benign order (BSM_PRIMITIVE_CLASS/CD_Class are assigned before the
    // line-249 read, so the proxy ctors see non-null statics). The
    // re-entrant hint fired from inside that nested init short-circuits
    // on the same-thread Initializing check, so recursion terminates.
    {
        let is_primitive_class_desc = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some_and(|c| &*c.name == "jdk/internal/constant/PrimitiveClassDescImpl");
        if is_primitive_class_desc {
            if let Ok(cd_id) = shared.load_class_concurrent("java/lang/constant/ConstantDescs") {
                let _ = ensure_class_initialized_shared(shared, thread, cd_id);
            }
        }
    }

    let current_thread_id = thread.thread_id.0;

    let class_state_name = if JvmThread::vm_state_diagnostics_enabled() {
        Some(
            shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {class_id}>")),
        )
    } else {
        None
    };

    loop {
        let (state, init_thread) = {
            let cm = shared.classes.class_manager.read();
            match cm.get_class(class_id) {
                Some(c) => (c.state, c.initializing_thread),
                None => (ClassState::Loaded, None),
            }
        };

        // If another thread has claimed this class for initialization (even if
        // the state hasn't transitioned to Initializing yet), treat it as if
        // it's in the Initializing state.
        let effective_state = if init_thread.is_some()
            && state != ClassState::Initialized
            && state != ClassState::InitializationError
            && state != ClassState::Initializing
        {
            ClassState::Initializing
        } else {
            state
        };

        match effective_state {
            ClassState::Initialized => {
                super::vm_object::pre_init_wrapper_type_field_for_class(shared, class_id);
                return Ok(());
            }

            ClassState::Initializing => {
                if init_thread == Some(current_thread_id) {
                    // Re-entrant: this thread is already initializing this class.
                    // JVM spec В§5.5 step 2: "If C is being initialized by the
                    // current thread, then this must be a recursive request for
                    // initialization. Release LC and complete normally."
                    return Ok(());
                }
                // Another thread is initializing this class вЂ” wait for it.
                // JVM spec В§5.5 step 2: "If C is being initialized by some
                // other thread, then release LC and block the current thread
                // until informed that the in-progress initialization has
                // completed."
                let waiter = shared
                    .classes
                    .class_init_waiters
                    .lock()
                    .get(&class_id)
                    .cloned();
                if let Some(pair) = waiter {
                    if let Some(name) = &class_state_name {
                        thread.set_vm_state(format!("class-init:wait:{name}"));
                    }
                    let (lock, cvar) = &*pair;
                    // GC-safety (the H2 TestScript three-way STW deadlock):
                    // the initializing thread may be parked at a GC
                    // safepoint inside <clinit> waiting for gc_complete;
                    // if WE park here while still counted in the barrier's
                    // `expected`, the GC initiator wedges in wait_for_all
                    // forever (and the 30s re-park loop never arrives).
                    // Run the full blocking-site protocol: deposit roots,
                    // retire the TLAB while its arena is still valid, mark
                    // GC-blocked (collections proceed and fold our frame
                    // fixups), wait, then re-sync on wake.
                    let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
                    // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE
                    // deposit - see vm_exec.rs::monitor_enter_blocking.
                    ctx.thread.tlab.retire();
                    ctx.deposit_root_snapshot();
                    let blk = shared.mem.gc_barrier.enter_blocked();
                    if blk.pre_stw {
                        // GCAUDIT-0711-FIX (finding 1a): auto - the deposit
                        // above already raised in_blocked_region.
                        let _ = shared.mem.gc_barrier.arrive_and_wait_auto(
                            crate::threading::jvm_thread::ThreadId(current_thread_id),
                        );
                    }
                    // Round-9 HIGH-4: parking_lot::Mutex + Condvar — no
                    // poison handling, wait_for returns a WaitTimeoutResult
                    // (no Result wrapper) since it cannot fail.
                    let mut guard = lock.lock();
                    // MISSED-NOTIFY FIX (2026-07-07 — the ~30s class-init
                    // stall behind the RRWL "crawl regime", the 21-28s
                    // Thread.join tails, and Elasticsearch startup stalls):
                    // the initializer sets `*done = true` under THIS mutex
                    // and only then calls `notify_all` (see
                    // `finalize_class_init`). If initialization completed
                    // between our class-state check above and this lock
                    // acquisition, that notify already fired — waiting
                    // unconditionally ate the full 30-second timeout, one
                    // straggler thread per incident (measured: recovery
                    // always lands at process lifetime ≈ +30s; probe join
                    // tails of 21.37s±15ms). Wait only while the done flag
                    // is unset; any wake (notify, timeout, spurious) falls
                    // through to the outer loop's state re-check.
                    if !*guard {
                        let _result = cvar.wait_for(&mut guard, std::time::Duration::from_secs(30));
                    }
                    drop(guard);
                    // `check_post_block_gc` clears the registry flag under
                    // the barrier admission lock before this thread resumes.
                    drop(blk);
                    ctx.check_post_block_gc();
                    // Loop back to re-check state (might be Initialized or Error)
                    continue;
                }
                // No waiter entry вЂ” init may have completed between our checks
                continue;
            }

            ClassState::InitializationError => {
                let class_name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("<unknown class {class_id}>"));
                // HIB-CV-26 fix (2026-07-16): JVMS §5.5 — re-triggering
                // initialization of a class that already failed to
                // initialize must raise a catchable `NoClassDefFoundError`,
                // not an unrecoverable `VmError::Internal`.
                // `raise_no_class_def_found` constructs the real Java
                // exception object (falling back to the old internal-error
                // form only if that construction itself fails, e.g. rt.jar
                // unavailable).
                return Err(crate::runtime::exceptions::raise_no_class_def_found(
                    shared,
                    thread,
                    &class_name,
                ));
            }

            _ => {
                // Atomically claim this class for initialization by this thread.
                if let Some(name) = &class_state_name {
                    thread.set_vm_state(format!("class-init:claim:{name}"));
                }
                // Use a write lock to prevent two threads from both entering
                // initialize_class_shared (TOCTOU race).
                let claimed = {
                    let mut cm = shared.classes.class_manager_write();
                    if let Some(class) = cm.get_class_mut(class_id) {
                        if class.initializing_thread.is_some() {
                            // Another thread claimed it between our read and this write
                            false
                        } else {
                            match class.state {
                                ClassState::Initialized => return Ok(()),
                                ClassState::Initializing => false,
                                ClassState::InitializationError => {
                                    let name = class.name.to_string();
                                    // HIB-CV-26 fix (2026-07-16): same JVMS
                                    // §5.5 fix as the fast-path check above —
                                    // raise a catchable
                                    // `NoClassDefFoundError` instead of an
                                    // internal error. `raise_no_class_def_found`
                                    // needs `shared.classes.class_manager` itself (to
                                    // resolve/allocate the exception object),
                                    // so the write-lock guard `cm` (which the
                                    // `class` borrow above is tied to) must be
                                    // released first — this non-reentrant
                                    // `RwLock` would otherwise self-deadlock.
                                    drop(cm);
                                    return Err(
                                        crate::runtime::exceptions::raise_no_class_def_found(
                                            shared, thread, &name,
                                        ),
                                    );
                                }
                                _ => {
                                    // Claim: set initializing_thread under the write lock.
                                    // The actual state transition to Initializing happens
                                    // later in initialize_class_shared after verification
                                    // and preparation, but this thread owns the class now.
                                    class.initializing_thread = Some(current_thread_id);
                                    // Register waiter so other threads can block immediately.
                                    // Round-9 HIGH-4: parking_lot::Mutex + Condvar (no poison).
                                    let waiter = std::sync::Arc::new((
                                        parking_lot::Mutex::new(false),
                                        parking_lot::Condvar::new(),
                                    ));
                                    shared
                                        .classes
                                        .class_init_waiters
                                        .lock()
                                        .insert(class_id, waiter);
                                    true
                                }
                            }
                        }
                    } else {
                        true
                    }
                };
                if claimed {
                    if let Some(name) = &class_state_name {
                        thread.set_vm_state(format!("class-init:initialize:{name}"));
                    }
                    clinit_order_trace(shared, thread, class_id, "claim");
                    let r = initialize_class_shared(shared, thread, class_id);
                    clinit_order_trace(
                        shared,
                        thread,
                        class_id,
                        if r.is_ok() { "done" } else { "FAIL" },
                    );
                    return r;
                }
                // Another thread claimed it вЂ” loop back to wait
                continue;
            }
        }
    }
}

/// Round-9 vm CRIT-1 / classloading CRIT-1 fix: atomic-only fast-path
/// init check that takes a `&Class` directly (NOT a `ClassId`).
///
/// Use this from call sites that already hold a borrow of the `Class`
/// (e.g. a `class_manager.read()` guard kept around for an adjacent
/// lookup): a single `Arc<AtomicU8>::load(Acquire)` answers
/// "is C fully initialized?" with NO further lock or hash probe.
///
/// Callers that only have a `ClassId` should use
/// [`ensure_class_initialized_shared`] (which performs the same fast
/// check internally after a brief `class_manager.read()` to resolve
/// `ClassId → &Class`) or [`is_class_initialized_via_manager`].
#[inline]
pub fn is_class_initialized_fast(class: &Class) -> bool {
    class.init_state.load(std::sync::atomic::Ordering::Acquire)
        == cratonvm_classloading::CLASS_INIT_INITIALIZED
}

thread_local! {
    /// Classes this host thread has already observed fully initialized, scoped
    /// to a `SharedVm` so sequential in-process VMs cannot alias `ClassId`s.
    ///
    /// Class initialization is monotonic: once a class reaches
    /// `CLASS_INIT_INITIALIZED` it stays there for the life of the VM (a failed
    /// `<clinit>` poisons the class into an *erroneous* state instead, which
    /// never reads as initialized). A positive answer is therefore permanently
    /// valid and needs no invalidation. Negative answers are NOT memoized —
    /// they change as soon as `<clinit>` completes.
    ///
    /// Without this, every JIT `getstatic` — `ensure_class_initialized_shared`
    /// runs on each one — acquired and released the process-wide
    /// `class_manager` `RwLock` just to read a per-class atomic. That lock
    /// acquire is an atomic read-modify-write on one shared cache line, so a
    /// hot loop containing a `getstatic` (e.g. `name.toLowerCase(Locale.ENGLISH)`,
    /// which reads `Locale.ENGLISH` every iteration) degraded sharply as soon
    /// as more than one mutator thread ran it.
    /// # A `Vec` with a 64-entry cap until 2026-08-19, and the cap was the bug
    ///
    /// The memo was linear-scanned and evicted FIFO with `remove(0)`. Below 64
    /// hot classes that is fine; above it the structure THRASHES -- every call
    /// scans all 64 entries, misses, takes the process-wide `class_manager`
    /// `RwLock` anyway, then pays an O(64) shift to evict. The memo stops
    /// paying for itself at exactly the workloads that need it most, and
    /// `is_class_initialized_via_manager` measured **3.11%** of
    /// `LegendreHighPrecisionTest`.
    ///
    /// An `FxHashSet` with no cap removes both costs. Unbounded is right here:
    /// an entry is one `(vm, ClassId)` pair, so the set is bounded by the
    /// number of classes the VM ever loads -- a few thousand at most, 12 bytes
    /// each, per thread that asks. Capping a structure whose key space is
    /// already bounded buys nothing and reintroduces the thrash.
    static CLASS_INITIALIZED_MEMO: std::cell::RefCell<rustc_hash::FxHashSet<(usize, u32)>> =
        std::cell::RefCell::new(rustc_hash::FxHashSet::default());
}

/// Round-9 vm CRIT-1 fix: convenience wrapper that takes a
/// `ClassId`, briefly holds `class_manager.read()` to resolve it to a
/// `&Class`, and probes the embedded atomic. Returns `false` for
/// unknown class ids (the slow path will then surface the appropriate
/// error).
///
/// This is the building block of [`ensure_class_initialized_shared`]'s
/// fast path. Exposed separately so callers that want a non-erroring
/// "is C initialized?" probe (e.g. JIT entry stubs deciding whether to
/// emit an init barrier) can use it without going through the
/// full Err-returning machinery.
#[inline]
pub fn is_class_initialized_via_manager(shared: &SharedVm, class_id: ClassId) -> bool {
    let vm_key = shared as *const SharedVm as usize;
    let raw = class_id.as_u32();
    if CLASS_INITIALIZED_MEMO.with(|memo| memo.borrow().contains(&(vm_key, raw))) {
        return true;
    }
    let initialized = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(class_id) {
            Some(class) => is_class_initialized_fast(class),
            None => false,
        }
    };
    if initialized {
        CLASS_INITIALIZED_MEMO.with(|memo| {
            memo.borrow_mut().insert((vm_key, raw));
        });
    }
    initialized
}

/// Decide whether a class is eligible to skip Pass 3 (bytecode) verification.
///
/// SECURITY (HIGH): the predicate MUST require BOTH
/// (a) the defining loader is the bootstrap loader, AND
/// (b) the class lives in a trusted JDK package prefix (`java/`, `jdk/`,
///     `sun/`, `com/sun/`).
///
/// Gating on the name alone would let a user classpath define
/// `java/lang/EvilString` (or any other class in a trusted prefix) and
/// bypass Pass 3 entirely. Combined with the interpreter's
/// `_unchecked` operand-stack helpers, an unverified class in a
/// trusted-prefix package can corrupt the operand stack and escape the
/// VM sandbox. JVMS В§5.3.1 reserves `java.*` for the bootstrap loader
/// in the spec, but CratonVM may still observe a non-bootstrap loader
/// defining such a class if the bytes are presented; we simply refuse
/// to take the verifier shortcut for it.
///
/// The skip itself is justified for *real* bootstrap classes: they are
/// loaded from jimage and have already been verified by javac/jlink, so
/// re-running Pass 3 just pays cost without finding anything.
#[inline]
fn verifier_skip_eligible(class: &Class) -> bool {
    // SECURITY FIX (V13): the verifier-skip MUST be gated on the actual
    // defining-classloader IDENTITY being the bootstrap loader, not on the
    // class name alone. `class.loader_id` is assigned by the VM at
    // define-class time from the loader that actually defined the class
    // (see `ClassManager::define_class*`); a user `ClassLoader.defineClass`
    // cannot forge `ClassLoaderId::Bootstrap` for itself. The name-prefix
    // check is retained only as a secondary narrowing — it is necessary but
    // NOT sufficient. A class in a trusted prefix (e.g. a forged
    // `java/lang/EvilString`) defined by a non-bootstrap loader fails the
    // identity check and is therefore verified, not skipped.
    //
    // This is the same trust predicate used by the strict-by-default
    // decision in `classloading::bytecode_verifier::class_is_bootstrap_trusted`
    // (SECURITY FIX V4), keeping the "trusted" determination consistent:
    // a class that is not skip-eligible here is exactly a class that the
    // bytecode verifier treats as untrusted and verifies strictly.
    let is_bootstrap_loaded = class.loader_id == cratonvm_types::ClassLoaderId::Bootstrap;
    if !is_bootstrap_loaded {
        // Identity gate failed: never skip, regardless of name prefix.
        return false;
    }
    let has_trusted_prefix = class.name.starts_with("java/")
        || class.name.starts_with("jdk/")
        || class.name.starts_with("sun/")
        || class.name.starts_with("com/sun/");
    has_trusted_prefix
}

/// Full class initialization sequence (JVM spec 5.5).
/// JVMS §5.5 step 7: initializing a class `C` recursively initializes each
/// superinterface (direct or indirect) of `C` that **declares a non-abstract,
/// non-static (i.e. default) method** — NOT every superinterface, and NOT one
/// that merely declares a static field. Returns true if `iface_id`'s transitive
/// superinterface closure (including itself) declares such a method.
///
/// The previous criterion ("declares a non-constant static field") over-eagerly
/// initialized interfaces like kotlin-reflect `KotlinTypeChecker` (static
/// `DEFAULT` field, no default methods): when `NewKotlinTypeCheckerImpl` was
/// constructed mid-`NewKotlinTypeChecker.<clinit>`, that re-entered
/// `KotlinTypeChecker.<clinit>`, which read the not-yet-assigned
/// `NewKotlinTypeChecker.Companion` as null → "Cannot invoke getDefault on null"
/// (SB-15, ReleaseScheduleTests). HotSpot uses the default-method criterion and
/// does not init `KotlinTypeChecker` there.
fn interface_has_default_method(shared: &SharedVm, iface_id: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let mut stack = vec![iface_id];
    let mut seen = rustc_hash::FxHashSet::default();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(c) = cm.get_class(id) {
            if c.methods.iter().any(|m| !m.is_abstract() && !m.is_static()) {
                return true;
            }
            for &s in &c.interfaces {
                stack.push(s);
            }
        }
    }
    false
}

/// Finalize a class initialization: set its final state, clear the
/// `initializing_thread` claim, keep the `AtomicU8` fast-path cache in sync, then
/// remove the waiter entry and notify every thread blocked on this class.
///
/// This is the single cleanup primitive used by [`initialize_class_shared`].
/// Both the success/explicit-error terminal paths and the RAII
/// [`InitCleanupGuard`] (which covers the early `?`/error returns) funnel through
/// here, so a `<clinit>` that fails part-way no longer leaks the init claim and
/// waiter — other threads are notified of the `InitializationError` immediately
/// instead of blocking for the full wait timeout.
///
/// Round 5 audit fix (HIGH): only `Initialized` flips the fast-path cache to the
/// fast-return state — `InitializationError` stays UNINITIALIZED so the fast path
/// always falls through to the slow path which converts it to
/// `NoClassDefFoundError`.
/// `CRATONVM_DBG_CLINIT_FAIL=1` -- print the class name and a Rust backtrace
/// the FIRST time a class is parked in `InitializationError`.
///
/// Every later use of that class raises a fresh `NoClassDefFoundError` from
/// `ensure_class_initialized_shared`, and that is all a
/// `CRATONVM_DBG_LINKAGE_BT` trace can show -- it names the consumer, never
/// the original failure. Without this lever a "NoClassDefFoundError for a
/// class that is plainly on the classpath" can only be chased by breakpoint.
fn dbg_clinit_fail() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CLINIT_FAIL").is_some())
}

fn finalize_class_init(shared: &SharedVm, class_id: ClassId, new_state: ClassState) {
    if matches!(new_state, ClassState::InitializationError) && dbg_clinit_fail() {
        let name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<class {class_id}>"));
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[DBG_CLINIT_FAIL] {name} -> InitializationError\n{bt}");
    }
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            class.state = new_state;
            class.initializing_thread = None;
        }
        if matches!(new_state, ClassState::Initialized) {
            cm.set_class_init_state(class_id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
        }
    }
    if matches!(new_state, ClassState::Initialized) {
        super::vm_object::pre_init_wrapper_type_field_for_class(shared, class_id);
        // Authoritative, monotonic "this class is initialized" event. The JIT's
        // lock-free init memo used to be written ONLY by `jit_getstatic`, i.e.
        // only after a compiled static read had already taken the slow path
        // once. That is too late for the compiler, which must decide whether a
        // `getstatic` may become a direct load (no helper, so no init check)
        // BEFORE the method's first compiled execution. Marking it here makes
        // the memo a general lock-free predicate with no extra bookkeeping:
        // this is exactly the point at which `<clinit>` has completed
        // successfully, which is the only condition the memo is allowed to
        // record (a failed `<clinit>` finalizes as `InitializationError` and is
        // deliberately not marked).
        crate::jit::helpers::note_class_initialized(shared, class_id);
    }
    // Remove waiter and notify all blocked threads.
    let removed = shared.classes.class_init_waiters.lock().remove(&class_id);
    if let Some(pair) = removed {
        let (lock, cvar) = &*pair;
        // Round-9 HIGH-4: parking_lot — no poison/unwrap.
        let mut done = lock.lock();
        *done = true;
        cvar.notify_all();
    }
}

/// RAII guard ensuring that a class claimed for initialization is always
/// finalized, even on the early `?`/`return Err(..)` paths inside
/// [`initialize_class_shared`] (superclass init, verification, preparation,
/// superinterface init, `System` stdin setup). If the guarded body returns
/// without reaching a terminal `finalize`, the guard runs
/// `finalize_class_init(.., InitializationError)` on drop — clearing the
/// `initializing_thread` claim, removing the waiter, and notifying all waiters of
/// the error state.
///
/// The guarded body disarms the guard (`finalized.set(true)`) at every terminal
/// `finalize` so the success path (and the explicit `InitializationError`
/// terminal path) never double-finalizes.
struct InitCleanupGuard<'a> {
    shared: &'a SharedVm,
    class_id: ClassId,
    /// Shared with the body's `finalize_init` closure; set to `true` once a
    /// terminal finalize has run so the guard's drop becomes a no-op.
    finalized: &'a std::cell::Cell<bool>,
}

impl Drop for InitCleanupGuard<'_> {
    fn drop(&mut self) {
        if !self.finalized.get() {
            // The body returned early without finalizing (a propagated `Err`
            // from superclass/interface init, verification, preparation, or the
            // System-streams setup, or a panic unwinding through the body).
            // Mark the class Erroneous and release every waiter promptly.
            finalize_class_init(self.shared, self.class_id, ClassState::InitializationError);
        }
    }
}

fn initialize_class_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    // Cleanup safety net: the caller (`ensure_class_initialized_shared`) has
    // already claimed this class (`initializing_thread` set) and registered a
    // waiter. Every return path below MUST clear that claim and notify waiters,
    // otherwise concurrent threads block until their wait timeout. The terminal
    // success/error paths do so explicitly via the `finalize_init` closure; this
    // guard covers the early `?`/`return Err(..)` paths (superclass init,
    // verification, preparation, superinterface init, `System` stdin) and any
    // panic unwinding through the body.
    let finalized = std::cell::Cell::new(false);
    let _init_guard = InitCleanupGuard {
        shared,
        class_id,
        finalized: &finalized,
    };

    // Step 1: Initialize the superclass first
    let superclass_id = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .and_then(|c| c.superclass);
    if let Some(super_id) = superclass_id {
        ensure_class_initialized_shared(shared, thread, super_id)?;
    }

    // Step 2: Structural verification (Loaded -> Verifying -> Verified)
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            if class.state == ClassState::Loaded {
                class.state = ClassState::Verifying;
            }
        }
    }
    // Run verification with read lock (unless -noverify).
    //
    // JVMS В§5.4.1 requires verification before linking. We always run Pass 2
    // (structural verification вЂ” access flag conflicts, final-class/method
    // constraints, abstract-method implementation, Code attribute presence),
    // which is cheap and well-tested on both synthetic and real .class files.
    //
    // Pass 3 (bytecode type-checking) runs at DEFINE time only
    // (`define_class_with_options`, class_manager.rs), where ClassManager's
    // loader-aware ClassStoreHierarchy resolves referenced names through the
    // defining loader's delegation order. It is deliberately not repeated
    // here; see the comment at the former call site below.
    if !shared.config.skip_verification {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        // A class DEFINED with `skip_verification` (trusted runtime-generated
        // bytecode — ByteBuddy / CGLIB / JDK Proxy / `Lookup.defineClass` /
        // `Unsafe.defineClass`) is deliberately not verified at define time
        // (`define_class_with_options`, class_manager.rs ~3267): our worklist
        // verifier cannot model its synthesised frames. Link-time MUST honor the
        // SAME per-class decision, otherwise the class our own define path
        // trusted is re-verified here and a structural rule it legitimately
        // bends is a HARD link error. Concretely: Hibernate asks ByteBuddy to
        // build a lazy-proxy `Entity$HibernateProxy` whose getter overrides a
        // FINAL accessor — HotSpot surfaces a *catchable* error the proxy
        // factory handles; our link-time Pass-2 (`verify_final_method_constraint`)
        // returned an UNCATCHABLE `InternalError` → process abort
        // (`FinalAccessorProxyFactoryTests`). Skipping here matches define-time
        // and HotSpot's "don't re-verify a trusted class" policy.
        let per_class_skip = cm.class_skip_bytecode_verification(class_id);
        if let Some(class) = store.get(class_id) {
            if class.state == ClassState::Verifying {
                // Skip verification for JDK bootstrap classes loaded
                // from jimage (they're pre-verified by javac/jlink).
                // This matches HotSpot's behavior: -Xverify:none for
                // java.base, -Xverify:remote for application classes.
                // `-Xverify:all` withdraws the bootstrap shortcut: the skip
                // below exists because jimage classes were pre-verified by
                // javac/jlink, and a deployment asking for `all` is asking us
                // to stop taking that on trust.
                let skip_boot = !cm.strict_verification() && verifier_skip_eligible(class);
                if !per_class_skip && !skip_boot {
                    // Pass 2 вЂ” structural verification.
                    let structural =
                        crate::classloading::verifier::verify_class_structure(class, store);
                    // Pass 3 (bytecode type-checking) is deliberately NOT
                    // repeated at link time. Define time already ran it with
                    // the loader-aware hierarchy; the adapter available here
                    // is only a name-indexed ClassStore (first match across
                    // ALL loaders). When an unrelated user-defined loader
                    // (e.g. Spring's per-test forked TestCompiler loader)
                    // also defines a same-named library class, is_subclass /
                    // is_direct_superclass walk the WRONG class's hierarchy
                    // and this re-check throws a spurious VerifyError for
                    // bytecode the authoritative define-time Pass 3 already
                    // accepted (seen on AssertJ's
                    // AbstractThrowableAssert.<init> super() call in the
                    // Spring AOT bean-registration cluster, 2026-07-13).
                    // UserDefined-loaded classes deliberately defer Pass 3 at
                    // define time for the same loader-fidelity reason
                    // (`defer_loader_sensitive_pass3`), so re-checking any
                    // loader's classes here with the naive hierarchy only
                    // reintroduces false rejections.
                    let bytecode = structural;
                    if let Err(e) = bytecode {
                        drop(cm);
                        // Cleanup (state -> InitializationError, clear the init
                        // claim, remove the waiter, notify all waiters) is run by
                        // `InitCleanupGuard::drop` since `finalized` is still
                        // false on this early return. Funnelling through the guard
                        // keeps a single cleanup point and, unlike the previous
                        // bare `state = InitializationError` write, also releases
                        // threads blocked on this class.
                        return Err(MethodCallFailed::InternalError(VmError::Linkage(e)));
                    }
                }
            }
        }
    }
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            if class.state == ClassState::Verifying {
                class.state = ClassState::Verified;
            }
        }
    }

    // Step 3: Preparation (Verified -> Preparing -> Prepared)
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            if class.state == ClassState::Verified {
                class.state = ClassState::Preparing;
            }
        }
    }
    prepare_class_shared(shared, class_id)?;
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            if class.state == ClassState::Preparing {
                class.state = ClassState::Prepared;
            }
        }
    }

    // Step 3.5: Initialize directly-implemented superinterfaces that declare a
    // non-abstract, non-static (default) method, per JVMS §5.5 step 7. The
    // criterion is *default methods*, NOT static fields: a superinterface that
    // only declares a static field is initialized lazily at the `getstatic`
    // that reads the field, never eagerly here. (See `interface_has_default_method`
    // doc — the old static-field criterion broke kotlin-reflect circular init.)
    {
        let iface_ids: Vec<ClassId> = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default();
        for iface_id in iface_ids {
            let not_inited = shared
                .classes
                .class_manager
                .read()
                .get_class(iface_id)
                .map(|iface| {
                    iface.state != ClassState::Initialized
                        && iface.state != ClassState::Initializing
                })
                .unwrap_or(false);
            if not_inited && interface_has_default_method(shared, iface_id) {
                ensure_class_initialized_shared(shared, thread, iface_id)?;
            }
        }
    }

    // Step 4: Execute <clinit> (Prepared -> Initializing -> Initialized)
    // Note: the Initializing state and waiter condvar are already set by the
    // caller (ensure_class_initialized_shared) to avoid TOCTOU races.
    // We still need to set the state here for the verification/preparation
    // steps above that may have changed it to Verified/Prepared.
    let current_thread_id = thread.thread_id.0;
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            class.state = ClassState::Initializing;
            class.initializing_thread = Some(current_thread_id);
        }
    }

    // Special hook: inject System.out/System.err into static fields.
    // The synthetic PrintStream objects live in SharedVm.system_out/system_err,
    // but GETSTATIC reads from SharedVm.classes.statics. We bridge them here so that
    // `System.out` resolves correctly via the normal static field path.
    {
        let class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone());
        if class_name.as_deref() == Some("java/lang/System") {
            let (out_ref, err_ref) = shared.ensure_system_streams();
            let in_ref = ensure_system_stdin_object(shared, thread)?;
            // Find the static field indices for "out", "err", and "in"
            let field_indices = {
                let cm = shared.classes.class_manager.read();
                if let Some(class) = cm.get_class(class_id) {
                    let mut out_idx = None;
                    let mut err_idx = None;
                    let mut in_idx = None;
                    let mut static_idx = 0usize;
                    for field in &class.fields {
                        if field.is_static() {
                            if &*field.name == "out" {
                                out_idx = Some(static_idx);
                            } else if &*field.name == "err" {
                                err_idx = Some(static_idx);
                            } else if &*field.name == "in" {
                                in_idx = Some(static_idx);
                            }
                            static_idx += 1;
                        }
                    }
                    (out_idx, err_idx, in_idx)
                } else {
                    (None, None, None)
                }
            };
            if let Some(out_idx) = field_indices.0 {
                super::set_static_shared(shared, class_id, out_idx, Value::Object(Some(out_ref)));
            }
            if let Some(err_idx) = field_indices.1 {
                super::set_static_shared(shared, class_id, err_idx, Value::Object(Some(err_ref)));
            }
            if let Some(in_idx) = field_indices.2 {
                super::set_static_shared(shared, class_id, in_idx, Value::Object(Some(in_ref)));
            }
        }
    }

    // Check if <clinit> exists (read lock only) вЂ” also check native registry
    // because synthetic stubs have no bytecode methods but may have native <clinit>.
    let has_clinit = {
        let in_class = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.find_method("<clinit>", "()V"))
            .is_some();
        if in_class {
            true
        } else {
            let class_name = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            shared
                .natives
                .native_methods
                .find(&class_name, "<clinit>", "()V")
                .is_some()
        }
    };

    // Get class name once before clinit (avoids lock ordering issues)
    let class_name_for_jfr = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    // FFM bootstrap guard: preparation pre-seeds the supported ValueLayout
    // statics (ADDRESS, JAVA_INT, unaligned variants, etc.) with synthetic
    // layout objects. Running the real JDK 25 interface <clinit> is unsafe in
    // Compatible mode because it rebuilds ADDRESS through Unsafe.ADDRESS_SIZE
    // before HotSpot-style UnsafeConstants backfill is available, producing
    // alignment 0 and poisoning SharedUtils.<clinit>. Treat the preseeded
    // interface as a no-<clinit> class so normal finalization/JFR handling
    // below still runs.
    //
    // JDK-ONLY-LAYOUT (step 3): under `CompatibilityMode::JdkOnly` the
    // suppression is LIFTED and the preseed at the bottom of `prepare_class` is
    // skipped, because the pair of them is a `CompatibilityClassRequested`
    // violation rather than a slot-numbering bug — see
    // `make_prepared_value_layout`, which invents two instance slots on an
    // object of an INTERFACE type that has none. The precondition the wave-2
    // requirement named is met: `post_clinit_fixup` backfills
    // `jdk/internal/misc/UnsafeConstants` with real platform values (its
    // "UnsafeConstants populated (5/5)" line) and `prepare_class` runs after
    // it, so the real `<clinit>` reads a true `ADDRESS_SIZE0`.
    let jdk_only = shared.compatibility_mode().is_jdk_only();
    let has_clinit =
        has_clinit && (jdk_only || &*class_name_for_jfr != "java/lang/foreign/ValueLayout");
    let init_start = std::time::Instant::now();

    // AOT training: record class load event for pre-linking
    #[cfg(feature = "experimental-aot")]
    {
        crate::native::builtins::aot::aot_record_class_loaded(&class_name_for_jfr);
    }

    // Helper: finalize initialization — set final state, clear tracking,
    // remove waiter entry, and notify all waiting threads. Delegates to the
    // shared `finalize_class_init` primitive and disarms `InitCleanupGuard` so
    // the guard's drop does not re-finalize (double-finalize would mark an
    // already-`Initialized` class `InitializationError`). Borrows `&finalized`
    // by shared ref — same `Cell` the guard borrows — so both coexist.
    let finalize_init = |shared: &SharedVm, class_id: ClassId, new_state: ClassState| {
        finalize_class_init(shared, class_id, new_state);
        finalized.set(true);
    };

    if crate::runtime::env_cache::modstatic_dbg()
        && &*class_name_for_jfr == "org/jboss/modules/Module"
    {
        eprintln!("MODSTATIC: Module init reached, has_clinit={has_clinit}");
    }
    if has_clinit {
        tracing::debug!(class = %class_name_for_jfr, "running <clinit>");
        let _clinit_depth_guard = ClinitDepthGuard::enter();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::invoke_on_class_shared(shared, thread, class_id, "<clinit>", "()V", &[])
        }));
        let result = match result {
            Ok(r) => r,
            Err(panic_info) => {
                // Convert panics (e.g. from pop_unchecked stack underflow) to errors
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic in <clinit>".to_string()
                };
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::NotImplemented { feature: msg },
                )))
            }
        };
        match result {
            Ok(_) => {
                finalize_init(shared, class_id, ClassState::Initialized);
                if crate::runtime::env_cache::modstatic_dbg()
                    && &*class_name_for_jfr == "org/jboss/modules/Module"
                {
                    let cm = shared.classes.class_manager.read();
                    if let Some(class) = cm.get_class(class_id) {
                        let mut sidx = 0usize;
                        for f in &class.fields {
                            if f.is_static() {
                                let v = super::get_static_shared(shared, class_id, sidx);
                                eprintln!(
                                    "MODSTATIC[{sidx}] {} {} = {:?}",
                                    f.name, f.descriptor, v
                                );
                                sidx += 1;
                            }
                        }
                    }
                    drop(cm);
                }
                // RBIGDEC.1 — even when `<clinit>` succeeds, java/math/BigInteger
                // (and a few other classes whose static constants are read by
                // synthetic-stub-style natives in `native-builtins/src/lib.rs`)
                // need a post-init patch: the natives read instance slot 0 as a
                // decimal String, but the real-JDK layout has `signum:I` there.
                // Without this overlay+descriptor-cache poison, every operation
                // on the static constants reads "0" through `bi_read`. The
                // post_clinit_fixup arm for these classes now detects existing
                // non-null statics and patches them in place rather than
                // overwriting (see make_or_patch_bi).
                if matches!(&*class_name_for_jfr, "java/math/BigInteger") {
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);
                }
                // Spring Boot `JarFileArchive` static init uses `EnumSet.of` /
                // `Set.of` on `PosixFilePermission.*`. A GETSTATIC/PUTSTATIC static
                // index mismatch can leave the enum constants null even after a
                // nominally successful `<clinit>` — mirror the BigInteger success
                // hook and backfill the nine constants from the real class layout.
                if matches!(
                    &*class_name_for_jfr,
                    "java/nio/file/attribute/PosixFilePermission"
                ) {
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);
                }
                // HIB-CV-27-linux-regression: `java/io/File.<clinit>` derives
                // separatorChar/separator/pathSeparatorChar/pathSeparator from
                // `DefaultFileSystem.getFileSystem()` (real `UnixFileSystem`/
                // `WinNTFileSystem`), whose constructor reads
                // `System.getProperties().getProperty("file.separator"/
                // "path.separator")` off the REAL `Properties.map`
                // (`ConcurrentHashMap`) field. The synthetic
                // `System.getProperties()` singleton is allocated via
                // `alloc_concurrent_synthetic` (no real constructor runs), so
                // `map` is null — `UnixFileSystem`'s field reads NPE partway
                // through, and per JVMS 5.5 a caught/swallowed `<clinit>`
                // failure leaves whichever static fields were assigned BEFORE
                // the throw (`separatorChar`/`separator`, set first) correct
                // while the ones after (`pathSeparatorChar`/`pathSeparator`)
                // stay at their zero-init default (`'\0'`/`""`). That silently
                // broke every real-bytecode consumer of `File.pathSeparator`
                // (e.g. `com.sun.tools.javac.file.Locations`'s default
                // `-classpath` decoding), which read an empty separator and
                // treated the whole `java.class.path` string as one jar path —
                // reproducing the exact "package X does not exist" symptom
                // HIB-CV-27 already fixed once, via a different `<clinit>`-order
                // (not encoding) mechanism. Backfill deterministically instead
                // of chasing the Properties bootstrap gap — these four fields
                // are pure platform constants, so an unconditional overwrite is
                // always correct (unlike the BigInteger/PosixFilePermission
                // arms, which patch in place because those constants are
                // themselves real heap objects the JDK allocates).
                if matches!(&*class_name_for_jfr, "java/io/File") {
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);
                }
                // FFM/Unsafe fix: `jdk/internal/misc/UnsafeConstants.<clinit>`
                // zero-inits ADDRESS_SIZE0/PAGE_SIZE/BIG_ENDIAN/UNALIGNED_ACCESS/
                // DATA_CACHE_LINE_FLUSH_SIZE and relies on the JVM to overwrite
                // them with the real platform values during bootstrap (HotSpot
                // does this natively). CratonVM never did, so `Unsafe.ADDRESS_SIZE`
                // (= UnsafeConstants.ADDRESS_SIZE0) stayed 0. That broke FFM:
                // `ValueLayout.<clinit>` builds the ADDRESS layout with
                // byteAlignment = Unsafe.ADDRESS_SIZE = 0 → IllegalArgumentException
                // "Invalid alignment: 0" → every FFM downcall binding's <clinit>
                // (e.g. Tomcat openssl_h) fails. Backfill the real values on the
                // success path, before `Unsafe.<clinit>` reads them.
                if matches!(&*class_name_for_jfr, "jdk/internal/misc/UnsafeConstants") {
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);
                }
                // ES-FAIL-FAMILY-20260710: see the matching `post_clinit_fixup`
                // arm for the full root-cause writeup. `Unsafe.<clinit>`
                // itself (not just `UnsafeConstants.<clinit>`) needs a
                // success-path backfill: its `ARRAY_*_BASE_OFFSET`/
                // `ARRAY_*_INDEX_SCALE` statics are computed by calling
                // natives that are not yet registered this early in boot,
                // so they silently latch at 0 instead of throwing.
                if matches!(&*class_name_for_jfr, "jdk/internal/misc/Unsafe") {
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);
                }
                // JDK 25's legacy sun.misc.Unsafe derives its memory-access
                // policy from the nested MemoryAccessOption enum during
                // <clinit>. Real-JDK execution can leave the final result
                // field null and can also skip loading the nested enum. Load
                // and initialize that enum only after Unsafe itself has been
                // finalized, then repair the slot before any ordered Unsafe
                // access reaches beforeMemoryAccessSlow(). Doing this inside
                // post_clinit_fixup used to deadlock because Unsafe was still
                // claimed as Initializing at that point.
                if matches!(&*class_name_for_jfr, "sun/misc/Unsafe") {
                    match shared.load_class_concurrent("sun/misc/Unsafe$MemoryAccessOption") {
                        Ok(enum_class_id) => {
                            if let Err(error) =
                                ensure_class_initialized_shared(shared, thread, enum_class_id)
                            {
                                tracing::warn!(
                                    "Post-clinit fixup: failed to initialize \
                                     sun.misc.Unsafe$MemoryAccessOption after Unsafe finalization: \
                                     {error:?}"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                "Post-clinit fixup: failed to load sun.misc.Unsafe$MemoryAccessOption \
                                 after Unsafe finalization: {error:?}"
                            );
                        }
                    }
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);
                }
                // (Removed) R15 WildFly Module.<clinit> post-success fixup.
                // The earlier band-aid unconditionally overwrote
                // `BOOT_MODULE_LOADER` (and conditionally backfilled
                // `systemPaths`/`systemPackages`) after a successful
                // `<clinit>`. Verified via instrumentation (2026-05-19) that
                // all of those statics are populated correctly by the real
                // JDK bytecode in `org/jboss/modules/Module.<clinit>`. The
                // band-aid was clobbering a real AtomicReference with a
                // fresh empty one, potentially desynchronizing module-loader
                // state. The synthetic-stubs policy forbids this kind of
                // overwrite; the Module arm is removed here and the matching
                // arm in `post_clinit_fixup` for `"org/jboss/modules/Module"`
                // is also gone.
                //
                // (Removed) KC26 `org/keycloak/common/Profile` /
                // `org/keycloak/config/FeatureOptions` post-clinit fixup.
                //
                // Earlier this arm called `keycloak_prepopulate_cache_statics`
                // which stamped `Profile.FEATURES` (and similarly-named cache
                // statics) with an *empty* `HashMap`. `Profile.FEATURES` is
                // not a `<clinit>`-populated field at all — it is lazily
                // built by `getOrderedFeatures()` on first access (`if
                // FEATURES == null { FEATURES = <built map> }`). Pre-stamping
                // it with an empty map made `getOrderedFeatures()` observe a
                // non-null `FEATURES` and return the empty map forever, so
                // `Profile.configure()` iterated zero feature entries, built
                // an empty per-instance `features` map, and published a
                // `CURRENT` profile that knows about no features. The first
                // `Profile.isFeatureEnabled(feature)` then did
                // `emptyMap.get(feature)` -> `null` ->
                // `((Boolean) null).booleanValue()` -> NPE at Profile.java:407.
                //
                // The fixup's stated rationale ("no-op LambdaMetafactory
                // CallSite makes the populating Stream pipeline loop forever")
                // is stale: `vm/src/runtime/invokedynamic.rs` implements
                // LambdaMetafactory.metafactory/altMetafactory for real, so
                // the `Stream.of(Feature.values()).forEach(...)` pipeline in
                // `getOrderedFeatures` runs normally. The synthetic empty-map
                // stamp violated the no-synthetic-stubs policy and *caused*
                // the NPE rather than preventing any hang. Removed so the real
                // `Profile` bytecode populates `FEATURES`.
                // Record JFR class load event
                let now_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                let duration_ns = init_start.elapsed().as_nanos() as u64;
                let mut jfr = shared.debug.flight_recorder.lock();
                // Round-9 HIGH-5 fix (2026-05-17): use `_arc` to skip the
                // per-event `Arc::from(&str)` clone — `class_name_for_jfr`
                // is already `Arc<str>` (cloned from `Class.name`).
                cratonvm_jfr::builtin::emit_class_load_event_arc(
                    &mut jfr,
                    class_name_for_jfr.clone(),
                    "app",
                    "app",
                    now_ns.saturating_sub(duration_ns),
                    duration_ns,
                );
                Ok(())
            }
            Err(e) => {
                if crate::runtime::env_cache::modstatic_dbg()
                    && &*class_name_for_jfr == "org/jboss/modules/Module"
                {
                    eprintln!("MODSTATIC: Module <clinit> FAILED: {:?}", &e);
                }
                // If <clinit> failed with a stack underflow (broken invokedynamic
                // in JDK internal classes), treat the class as initialized anyway.
                // This happens for JDK classes like jdk.internal.util.Preconditions
                // whose <clinit> uses LambdaMetafactory invokedynamic which we
                // don't fully support. The native overrides provide the actual
                // functionality these classes need.
                let is_stack_underflow = matches!(
                    &e,
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::NotImplemented { feature }
                    )) if feature.contains("stack underflow")
                        || feature.contains("stack overflow")
                        || feature.contains("invokedynamic")
                );
                // NARROWED (2026-06-17): only swallow the stack-error /
                // invokedynamic failure for classes with a documented recovery
                // path (`clinit_swallow_has_recovery`). Previously this
                // swallowed for ANY class — hiding broken invokedynamic /
                // LambdaMetafactory bugs in arbitrary code behind a
                // stamped-`Initialized` class with null statics. For everything
                // else the failure now falls through to the JVMS-correct
                // propagation below.
                if is_stack_underflow
                    && lenient_clinit()
                    && clinit_swallow_has_recovery(&class_name_for_jfr)
                {
                    tracing::warn!(
                        class = %class_name_for_jfr,
                        "CRATONVM_LENIENT_CLINIT: swallowing <clinit> stack-error/invokedynamic failure and marking Initialized (JVMS-divergent; class has documented recovery path)"
                    );
                    crate::runtime::diagnostics::record_swallow(
                        shared,
                        "<clinit>",
                        "stack-error/invokedynamic",
                        &format!("class={} err={:?}", class_name_for_jfr, &e),
                    );
                    finalize_init(shared, class_id, ClassState::Initialized);
                    return Ok(());
                }
                // Debug-gated visibility: a stack-error/invokedynamic <clinit>
                // failure we are NOT swallowing (no recovery path). Name the
                // class+exception so the real LambdaMetafactory/invokedynamic
                // gap is visible at its true origin instead of silently
                // propagating as ExceptionInInitializerError.
                if is_stack_underflow
                    && lenient_clinit()
                    && crate::runtime::env_cache::strict_swallows()
                {
                    eprintln!(
                        "CRATONVM_LENIENT_CLINIT: NOT swallowing <clinit> stack-error/invokedynamic failure (no recovery path) — class={} err={:?} — propagating per JVMS §5.5",
                        class_name_for_jfr, &e
                    );
                }
                // For non-critical exception types during <clinit> (ClassCastException,
                // NullPointerException, etc.) that arise from incomplete native
                // implementations, swallow and mark as initialized so bootstrap
                // can proceed.
                // Swallow non-critical exceptions during <clinit> for classes
                // that aren't core VM infrastructure. This lets bootstrap proceed
                // when optional JDK/library classes have init failures due to
                // incomplete native support.
                let is_swallowable = if let MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                    let eid = shared.mem.heap.class_id_of(*exc_ref);
                    let cm = shared.classes.class_manager.read();
                    let exc_name = cm
                        .get_class(eid)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let is_swallowable_type = matches!(
                        &*exc_name,
                        "java/lang/ClassCastException"
                        | "java/lang/NullPointerException"
                        | "java/lang/UnsupportedOperationException"
                        | "java/lang/IllegalArgumentException"
                        | "java/lang/IllegalCallerException"
                        | "java/lang/IllegalAccessException"
                        | "java/lang/IllegalStateException"
                        | "java/lang/ArrayIndexOutOfBoundsException"
                        | "java/lang/StringIndexOutOfBoundsException"
                        | "java/lang/NumberFormatException"
                        | "java/lang/ArithmeticException"
                        | "java/lang/NoSuchMethodException"
                        | "java/lang/NoSuchFieldException"
                        | "java/lang/NoSuchMethodError"
                        | "java/lang/NoSuchFieldError"
                        | "java/lang/AbstractMethodError"
                        | "java/lang/IncompatibleClassChangeError"
                        | "java/lang/UnsatisfiedLinkError"
                        // Bare `java/lang/Error` (no specific subclass) — used
                        // by some optional-classpath probes (e.g. logback's
                        // ContextSelectorStaticBinder.<clinit> when the
                        // logback config is absent or partially parseable in
                        // CratonVM real-JDK mode). Treated as recoverable
                        // for the same framework allowlist below.
                        | "java/lang/Error"
                    );
                    // NARROWED (2026-06-17): the swallow is now gated on
                    // `clinit_swallow_has_recovery` — the explicit list of
                    // classes/packages that have a documented recovery path
                    // (a `post_clinit_fixup` arm, or the SLF4J/logback native
                    // binder stubs). The previous broad prefix allowlist
                    // (`java/`, `jdk/`, `sun/`, `javax/`, `org/jboss/`,
                    // `io/quarkus/`, `org/wildfly/`, `io/smallrye/`,
                    // `org/springframework/boot/loader/`, `com/sun/`, …)
                    // swallowed for EVERY framework class on a "non-critical"
                    // exception even when nothing repaired the
                    // half-initialized statics, hiding real init bugs behind a
                    // stamped-`Initialized` class with null statics that
                    // surfaced later as a bare NPE far from the cause. See
                    // `clinit_swallow_has_recovery` for the retained cases and
                    // the rationale for each.
                    let has_recovery = clinit_swallow_has_recovery(&class_name_for_jfr);
                    is_swallowable_type && has_recovery
                } else {
                    // NARROWED (2026-06-17): the internal-error swallow path
                    // (ClassCast/IllegalState/NPE/NotImplemented raised as a
                    // VM-internal `RuntimeError`, not a thrown Java exception)
                    // previously swallowed for ANY class — including arbitrary
                    // app classes — leaving them stamped `Initialized` with
                    // null statics. Gate it on the same `clinit_swallow_has_recovery`
                    // allowlist as the thrown-exception branch so only classes
                    // with a documented recovery path are tolerated.
                    let is_swallowable_internal = matches!(
                        &e,
                        MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::ClassCastException { .. }
                        )) | MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::IllegalStateException { .. }
                        )) | MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::NullPointerException { .. }
                        )) | MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::NotImplemented { .. }
                        ))
                    );
                    is_swallowable_internal && clinit_swallow_has_recovery(&class_name_for_jfr)
                };
                // JVMS §5.5 default: a failing `<clinit>` is *not* swallowed —
                // the class becomes Erroneous and the failure propagates (see
                // the fall-through below). The lenient framework-bootstrap mode
                // (swallow + `post_clinit_fixup` synthetic backfill) is opt-in
                // via `CRATONVM_LENIENT_CLINIT=1`. See the module doc comment.
                if is_swallowable && lenient_clinit() {
                    tracing::warn!(
                        class = %class_name_for_jfr,
                        "CRATONVM_LENIENT_CLINIT: swallowing <clinit> failure, marking Initialized + running post_clinit_fixup (JVMS-divergent)"
                    );
                    // Report the exception's class name AND its
                    // detailMessage (field 0 on Throwable subclasses).
                    // Earlier sessions avoided reading the message because
                    // a stale exc_ref could segfault on heap traversal;
                    // the current code path has stabilized, so include the
                    // message to aid KC16-class diagnostics.  Fall back to
                    // the class name alone on any read failure.
                    let exc_detail = match &e {
                        MethodCallFailed::ExceptionThrown(exc_ref) => {
                            let exc_cid = shared.mem.heap.class_id_of(*exc_ref);
                            let exc_class = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(exc_cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| format!("class_id={}", exc_cid.as_u32()));
                            let msg = match shared.mem.heap.get_field(*exc_ref, 0) {
                                crate::types::Value::Object(Some(s)) => {
                                    crate::vm::vm_object::read_java_string(&shared.mem.heap, s)
                                        .unwrap_or_default()
                                }
                                _ => String::new(),
                            };
                            if msg.is_empty() {
                                exc_class
                            } else {
                                format!("{}: {}", exc_class, msg)
                            }
                        }
                        other => format!("{:?}", other),
                    };
                    // KC16 RKC16N.14 — `java/math/BigDecimal.<clinit>` is known
                    // to NPE in real-JDK mode partway through (the JDK init
                    // dereferences `signum` on an internal helper return that
                    // our interpreter currently mis-resolves to null).  The
                    // `post_clinit_fixup` arm below fully repairs the public
                    // surface (ZERO/ONE/TWO/TEN) and downstream code runs
                    // correctly.  Because the cascade is fully recovered, we
                    // suppress the WARN line for this specific class — the
                    // operator-visible "BigDecimal swallow" log line that the
                    // KC16 boot map calls out as a blocker simply disappears.
                    // The swallow counter is still incremented so
                    // `CRATONVM_STRICT_SWALLOWS=1` continues to escalate (the
                    // gate is preserved for callers actively triaging this
                    // path) and the "main() completed with N swallowed VM
                    // error(s)" tally remains accurate.
                    let recoverable_silent = &*class_name_for_jfr == "java/math/BigDecimal";
                    if recoverable_silent {
                        shared
                            .debug
                            .swallow_counter
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if crate::runtime::env_cache::strict_swallows() {
                            panic!(
                                "CRATONVM_STRICT_SWALLOWS=1: swallow at <clinit> [non-critical-exception]: class={} exc={}",
                                class_name_for_jfr, exc_detail
                            );
                        }
                    } else {
                        // CRATONVM_DBG_CATALINA — log every <clinit> error
                        // swallowed during Tomcat-related class init. A
                        // swallowed clinit in a class `Catalina` depends on
                        // leaves a static field null and surfaces later as the
                        // bare NPE that aborts `Catalina.<clinit>`.
                        if cratonvm_types::flags::runtime_var("CRATONVM_DBG_CATALINA").is_ok()
                            && (class_name_for_jfr.starts_with("org/apache/catalina/")
                                || class_name_for_jfr.starts_with("org/apache/tomcat/")
                                || class_name_for_jfr.starts_with("org/apache/coyote/")
                                || class_name_for_jfr.starts_with("org/apache/juli/")
                                || class_name_for_jfr.starts_with("java/util/")
                                || class_name_for_jfr.starts_with("sun/util/"))
                        {
                            eprintln!(
                                "CATALINA-DBG: SWALLOWED <clinit> error class={} exc={} \
                                 — a static field of this class is now NULL",
                                class_name_for_jfr, exc_detail
                            );
                        }
                        crate::runtime::diagnostics::record_swallow(
                            shared,
                            "<clinit>",
                            "non-critical-exception",
                            &format!("class={} exc={}", class_name_for_jfr, exc_detail),
                        );
                        // Diagnostic: dump captured stack trace for the
                        // swallowed clinit exception so we can pinpoint where
                        // bare-NPEs originate during boot.
                        if let MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                            let h = shared.mem.heap.identity_hash_code(*exc_ref);
                            if let Some(frames) = shared.throwable_stack_trace(h) {
                                for (i, f) in frames.iter().enumerate().take(20) {
                                    tracing::warn!(
                                        "  [SWALLOW-TRACE {}] at {}.{} ({}:{}) bci={}",
                                        i,
                                        f.class_name,
                                        f.method_name,
                                        f.source_file.as_deref().unwrap_or("?"),
                                        f.line_number,
                                        f.byte_code_index,
                                    );
                                }
                            } else {
                                let cm = shared.classes.class_manager.read();
                                for (i, f) in thread.frames.iter().enumerate().rev().take(25) {
                                    let cn = cm
                                        .get_class(f.class_id)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    tracing::warn!(
                                        "  [SWALLOW-LIVE {}] class={} {}.{} pc={}",
                                        i,
                                        class_name_for_jfr,
                                        cn,
                                        f.method_name(),
                                        f.pc,
                                    );
                                }
                            }
                        }
                    }
                    finalize_init(shared, class_id, ClassState::Initialized);

                    // Post-clinit fixup: populate critical static fields that
                    // downstream code expects to be non-null.
                    post_clinit_fixup(shared, class_id, &class_name_for_jfr);

                    return Ok(());
                }
                // Debug-gated visibility (2026-06-17): lenient mode is ON but
                // the swallow was DECLINED — either the class has no documented
                // recovery path (`clinit_swallow_has_recovery` == false) or the
                // exception type is genuinely critical (StackOverflowError,
                // OutOfMemoryError, …, not on the recoverable list). Name the
                // class + exception type so a previously-hidden init bug is now
                // visible at its true origin instead of vanishing behind a
                // stamped-`Initialized` class. The failure then propagates per
                // JVMS §5.5 below. Gated on `CRATONVM_STRICT_SWALLOWS=1` to keep
                // the default lenient run quiet.
                // `CRATONVM_DBG_CLINIT_FAIL=1` names the exception here too --
                // the class-name-only report at `finalize_class_init` says
                // WHICH class died but never WHY, and the pre-existing
                // strict-swallows report only fires in lenient mode.
                if dbg_clinit_fail()
                    || (lenient_clinit() && crate::runtime::env_cache::strict_swallows())
                {
                    let exc_ty = match &e {
                        MethodCallFailed::ExceptionThrown(exc_ref) => {
                            let eid = shared.mem.heap.class_id_of(*exc_ref);
                            shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(eid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| format!("class_id={}", eid.as_u32()))
                        }
                        other => format!("{:?}", other),
                    };
                    eprintln!(
                        "[DBG_CLINIT_FAIL] <clinit> failure NOT swallowed (no recovery path / critical exception) — class={} exc={} — propagating per JVMS §5.5",
                        class_name_for_jfr, exc_ty
                    );
                }
                finalize_init(shared, class_id, ClassState::InitializationError);
                // JVM spec В§5.5: If the exception is an Error (or subclass),
                // propagate as-is. Otherwise, wrap in ExceptionInInitializerError.
                match &e {
                    MethodCallFailed::ExceptionThrown(exc_ref) => {
                        let exc_class_id = shared.mem.heap.class_id_of(*exc_ref);
                        let is_error = {
                            let cm = shared.classes.class_manager.read();
                            let error_id = cm.find_bootstrap_class_by_name("java/lang/Error");
                            match error_id {
                                Some(eid) => cm.is_subclass_of(exc_class_id, eid),
                                None => false,
                            }
                        };
                        if is_error {
                            Err(e)
                        } else {
                            let cause_ref = *exc_ref;
                            // Log the root cause for diagnostics. Read the
                            // detailMessage field (slot 0 on Throwable) so the
                            // operator can see WHICH field/argument was null
                            // вЂ” required to triage real-app `<clinit>` NPEs
                            // (e.g. KC26 `org/keycloak/common/Version`) where
                            // the bare class+cause pair gives no actionable
                            // signal. Read failure (e.g. stale ref or layout
                            // drift) falls back to empty string.
                            {
                                let cause_class = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(exc_class_id)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default();
                                // Read detailMessage by walking the field
                                // hierarchy by name — slot index varies because
                                // Throwable has `backtrace` at slot 0 and
                                // `detailMessage` at slot 1, and subclasses may
                                // have arbitrary layouts. Reading slot 0
                                // unconditionally returns `backtrace` (a
                                // non-String) for most exceptions, so the
                                // message field looked empty.
                                let cause_msg = {
                                    let cm = shared.classes.class_manager.read();
                                    let mut walk = Some(exc_class_id);
                                    let mut found: Option<crate::types::ObjectRef> = None;
                                    while let Some(cid) = walk {
                                        let Some(cls) = cm.get_class(cid) else { break };
                                        let mut inst = 0usize;
                                        for f in &cls.fields {
                                            if f.is_static() {
                                                continue;
                                            }
                                            if &*f.name == "detailMessage" {
                                                let idx = cls.first_field_index + inst;
                                                if let crate::types::Value::Object(Some(s)) =
                                                    shared.mem.heap.get_field(*exc_ref, idx)
                                                {
                                                    found = Some(s);
                                                }
                                                break;
                                            }
                                            inst += 1;
                                        }
                                        if found.is_some() {
                                            break;
                                        }
                                        walk = cls.superclass;
                                    }
                                    drop(cm);
                                    found
                                        .and_then(|s| {
                                            crate::vm::vm_object::read_java_string(
                                                &shared.mem.heap,
                                                s,
                                            )
                                        })
                                        .unwrap_or_default()
                                };
                                tracing::warn!(
                                    class = %class_name_for_jfr,
                                    cause = %cause_class,
                                    message = %cause_msg,
                                    "<clinit> failed вЂ” wrapping in ExceptionInInitializerError"
                                );
                                // Diagnostic: dump captured stack trace from
                                // the VM-wide Throwable trace registry so we can pinpoint where
                                // bare-NPEs originate during boot.
                                let h = shared.mem.heap.identity_hash_code(*exc_ref);
                                if let Some(frames) = shared.throwable_stack_trace(h) {
                                    for (i, f) in frames.iter().enumerate().take(20) {
                                        tracing::warn!(
                                            "  [CLINIT-TRACE {}] at {}.{} ({}:{}) bci={}",
                                            i,
                                            f.class_name,
                                            f.method_name,
                                            f.source_file.as_deref().unwrap_or("?"),
                                            f.line_number,
                                            f.byte_code_index,
                                        );
                                    }
                                } else {
                                    tracing::warn!("  [CLINIT-TRACE] no captured frames for hash={} — falling back to live thread.frames", h);
                                    // Fallback: live frames captured at the moment
                                    // we observe propagation. Even though some
                                    // frames have already been popped via athrow
                                    // unwinding, the deepest remaining frame is
                                    // typically the <clinit> we are about to
                                    // wrap, plus all surviving callers. This
                                    // beats no info at all.
                                    let cm = shared.classes.class_manager.read();
                                    for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                                        let cn = cm
                                            .get_class(f.class_id)
                                            .map(|c| c.name.to_string())
                                            .unwrap_or_default();
                                        tracing::warn!(
                                            "  [CLINIT-LIVE {}] at {}.{} pc={}",
                                            i,
                                            cn,
                                            f.method_name(),
                                            f.pc,
                                        );
                                    }
                                }
                                // Walk the cause chain — many JDK/log4j
                                // wrappers re-throw RuntimeException over a
                                // deeper NPE/IAE. Print up to 4 levels.
                                {
                                    let cm = shared.classes.class_manager.read();
                                    // Resolve a `java/lang/Throwable` field index
                                    // by NAME, walking the superclass chain.
                                    //
                                    // JDK-ONLY-LAYOUT: converted from raw slot
                                    // indices. A synthetic `java/lang/Throwable`
                                    // stub is `instance_fields(2)` — `_f0` = message,
                                    // `_f1` = cause — but the REAL JDK declares
                                    // `backtrace`, `detailMessage`, `cause`,
                                    // `stackTrace`, `depth`, `suppressedExceptions`,
                                    // so real slot 0 is `backtrace` and the message
                                    // lives at slot 1. Reading slot 0 as the message
                                    // against real bytes is a silent wrong-field read.
                                    let field_idx_of =
                                        |obj: crate::types::ObjectRef,
                                         want: &'static str|
                                         -> Option<usize> {
                                            let mut walk = Some(shared.mem.heap.class_id_of(obj));
                                            while let Some(k) = walk {
                                                if let Some(cls) = cm.get_class(k) {
                                                    let mut inst = 0usize;
                                                    for f in &cls.fields {
                                                        if !f.is_static() {
                                                            if &*f.name == want {
                                                                return Some(
                                                                    cls.first_field_index + inst,
                                                                );
                                                            }
                                                            inst += 1;
                                                        }
                                                    }
                                                    walk = cls.superclass;
                                                } else {
                                                    break;
                                                }
                                            }
                                            None
                                        };
                                    let mut cur = *exc_ref;
                                    for depth in 0..4 {
                                        let Some(ci) = field_idx_of(cur, "cause") else {
                                            break;
                                        };
                                        let cause_val = shared.mem.heap.get_field(cur, ci);
                                        let cause_obj = match cause_val {
                                            Value::Object(Some(o)) if o != cur => o,
                                            _ => break,
                                        };
                                        let cause_cid = shared.mem.heap.class_id_of(cause_obj);
                                        let cause_name = cm
                                            .get_class(cause_cid)
                                            .map(|c| c.name.to_string())
                                            .unwrap_or_default();
                                        // `detailMessage` by name on real bytes;
                                        // slot 0 only as the synthetic-stub
                                        // fallback (where the stub's `_f0` IS the
                                        // message and no name resolves).
                                        let msg_idx =
                                            field_idx_of(cause_obj, "detailMessage").unwrap_or(0);
                                        let cause_msg =
                                            match shared.mem.heap.get_field(cause_obj, msg_idx) {
                                                Value::Object(Some(s)) => {
                                                    crate::vm::vm_object::read_java_string(
                                                        &shared.mem.heap,
                                                        s,
                                                    )
                                                    .unwrap_or_default()
                                                }
                                                _ => String::new(),
                                            };
                                        tracing::warn!(
                                            "  [CLINIT-CAUSE depth={}] {}: {}",
                                            depth,
                                            cause_name,
                                            cause_msg,
                                        );
                                        let ch = shared.mem.heap.identity_hash_code(cause_obj);
                                        if let Some(frames) = shared.throwable_stack_trace(ch) {
                                            for (i, f) in frames.iter().enumerate().take(15) {
                                                tracing::warn!(
                                                    "    [CAUSE-TRACE {}] at {}.{} ({}:{}) bci={}",
                                                    i,
                                                    f.class_name,
                                                    f.method_name,
                                                    f.source_file.as_deref().unwrap_or("?"),
                                                    f.line_number,
                                                    f.byte_code_index,
                                                );
                                            }
                                        }
                                        cur = cause_obj;
                                    }
                                }
                            }
                            match crate::runtime::exceptions::create_exception_object(
                                shared,
                                thread,
                                "java/lang/ExceptionInInitializerError",
                                None,
                            ) {
                                Ok(eiie_ref) => {
                                    // Set the cause field directly on Throwable (field "cause")
                                    // so the stack-trace printer can follow the chain.
                                    let cause_idx = {
                                        let cm = shared.classes.class_manager.read();
                                        let mut found: Option<usize> = None;
                                        let mut walk = Some(shared.mem.heap.class_id_of(eiie_ref));
                                        while let Some(k) = walk {
                                            if let Some(cls) = cm.get_class(k) {
                                                let mut inst = 0usize;
                                                for f in &cls.fields {
                                                    if !f.is_static() {
                                                        if &*f.name == "cause" {
                                                            found =
                                                                Some(cls.first_field_index + inst);
                                                            break;
                                                        }
                                                        inst += 1;
                                                    }
                                                }
                                                if found.is_some() {
                                                    break;
                                                }
                                                walk = cls.superclass;
                                            } else {
                                                break;
                                            }
                                        }
                                        found
                                    };
                                    if let Some(i) = cause_idx {
                                        shared.mem.heap.set_field(
                                            eiie_ref,
                                            i,
                                            Value::Object(Some(cause_ref)),
                                        );
                                    }
                                    let _ = super::invoke_on_class_shared(
                                        shared,
                                        thread,
                                        shared.mem.heap.class_id_of(eiie_ref),
                                        "initCause",
                                        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                                        &[
                                            Value::Object(Some(eiie_ref)),
                                            Value::Object(Some(cause_ref)),
                                        ],
                                    );
                                    Err(MethodCallFailed::ExceptionThrown(eiie_ref))
                                }
                                Err(_) => Err(e),
                            }
                        }
                    }
                    _ => Err(e),
                }
            }
        }
    } else {
        // No <clinit> вЂ” class is fully initialized
        finalize_init(shared, class_id, ClassState::Initialized);

        // WP8.10.5 — synthetic-stub classes never run a <clinit> (they
        // have no bytecode), so the post-clinit fixup that populates
        // load-bearing static fields like
        // `DefaultBootModuleLoaderHolder.INSTANCE` was previously
        // unreachable for the synthetic-stub-only path.  Fire it here
        // for synthetic stubs only — real classes either run a clinit
        // (and reach the swallow-path fixup site at line ~543) or
        // populate their own statics.  Idempotent re-firing is fine
        // because every fixup arm only writes a fresh value when the
        // existing value is null/uninitialized.
        let is_synthetic_stub = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.origin.is_compatibility_stub())
            .unwrap_or(false);
        if is_synthetic_stub {
            post_clinit_fixup(shared, class_id, &class_name_for_jfr);
        }

        // Record JFR class load event
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let duration_ns = init_start.elapsed().as_nanos() as u64;
        let mut jfr = shared.debug.flight_recorder.lock();
        // Round-9 HIGH-5 fix (2026-05-17): use `_arc` variant — name is
        // already `Arc<str>` from `Class.name.clone()`.
        cratonvm_jfr::builtin::emit_class_load_event_arc(
            &mut jfr,
            class_name_for_jfr.clone(),
            "app",
            "app",
            now_ns.saturating_sub(duration_ns),
            duration_ns,
        );
        Ok(())
    }
}

/// Prepare a class: allocate static fields with default values, then apply
/// `ConstantValue` attributes (JVMS В§5.5 step 9 вЂ” must run before `<clinit>`
/// so compile-time constants are visible to the static initializer).
///
/// Supported constant pool entry types per JVMS В§4.7.2:
/// - `Integer` в†’ byte/char/short/int/boolean primitive slot
/// - `Float`   в†’ float slot
/// - `Long`    в†’ long slot
/// - `Double`  в†’ double slot
/// - `String`  в†’ resolves the Utf8, allocates a Java String via the VM's
///   string pool, and stores the reference in the slot.
/// The CONCRETE carrier for each preseeded `ValueLayout` static, with its size
/// and alignment.
///
/// These were the INTERFACE names until 2026-08-29, which is what made the
/// preseed a fabrication rather than a layout: an interface declares no
/// instance fields, so the two slots it wrote were invented. HotSpot answers
/// `jdk.internal.foreign.layout.ValueLayouts$OfIntImpl` for
/// `ValueLayout.JAVA_INT.getClass()`, and so does the strict arm, which drops
/// this preseed entirely.
fn value_layout_preseed(field_name: &str) -> Option<(&'static str, i64, i64)> {
    const OF_BYTE: &str = "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl";
    const OF_BOOLEAN: &str = "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl";
    const OF_CHAR: &str = "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl";
    const OF_SHORT: &str = "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl";
    const OF_INT: &str = "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl";
    const OF_LONG: &str = "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl";
    const OF_FLOAT: &str = "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl";
    const OF_DOUBLE: &str = "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl";
    const OF_ADDRESS: &str = "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl";
    match field_name {
        "ADDRESS" => Some((OF_ADDRESS, 8, 8)),
        "JAVA_BYTE" => Some((OF_BYTE, 1, 1)),
        "JAVA_BOOLEAN" => Some((OF_BOOLEAN, 1, 1)),
        "JAVA_CHAR" => Some((OF_CHAR, 2, 2)),
        "JAVA_SHORT" => Some((OF_SHORT, 2, 2)),
        "JAVA_INT" => Some((OF_INT, 4, 4)),
        "JAVA_LONG" => Some((OF_LONG, 8, 8)),
        "JAVA_FLOAT" => Some((OF_FLOAT, 4, 4)),
        "JAVA_DOUBLE" => Some((OF_DOUBLE, 8, 8)),
        "ADDRESS_UNALIGNED" => Some((OF_ADDRESS, 8, 1)),
        "JAVA_CHAR_UNALIGNED" => Some((OF_CHAR, 2, 1)),
        "JAVA_SHORT_UNALIGNED" => Some((OF_SHORT, 2, 1)),
        "JAVA_INT_UNALIGNED" => Some((OF_INT, 4, 1)),
        "JAVA_LONG_UNALIGNED" => Some((OF_LONG, 8, 1)),
        "JAVA_FLOAT_UNALIGNED" => Some((OF_FLOAT, 4, 1)),
        "JAVA_DOUBLE_UNALIGNED" => Some((OF_DOUBLE, 8, 1)),
        _ => None,
    }
}

fn make_prepared_value_layout(
    shared: &SharedVm,
    class_name: &str,
    byte_size: i64,
    byte_alignment: i64,
) -> Option<ObjectRef> {
    let layout_class_id = shared.load_class_concurrent(class_name).ok()?;
    let num_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(layout_class_id)
        .map(|c| c.num_total_fields.max(2))
        .unwrap_or(2);
    let obj = shared.mem.heap.alloc_object(layout_class_id, num_fields);
    // BY NAME, now that the carrier is a class that declares the fields.
    // `AbstractLayout` gives every layout `byteSize`, `byteAlignment` and
    // `name`; the positional 0/1 below is the fallback for a build that cannot
    // load the impl class, which is the old behaviour exactly.
    let named = {
        let cm = shared.classes.class_manager.read();
        let idx = |f: &str| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(
                layout_class_id,
                f,
                &cm.class_store,
            )
        };
        match (idx("byteSize"), idx("byteAlignment")) {
            (Some(size_slot), Some(align_slot)) => Some((size_slot, align_slot, idx("name"))),
            _ => None,
        }
    };
    if let Some((size_slot, align_slot, name_slot)) = named {
        shared.mem.heap.set_field(obj, size_slot, Value::Long(byte_size));
        shared
            .mem
            .heap
            .set_field(obj, align_slot, Value::Long(byte_alignment));
        // EXPLICITLY null, not merely unwritten: an untouched reference slot
        // reads back through the R-niche rule as `Int(0)`, which `name()` would
        // then wrap into a non-empty `Optional`. That is the same decoding that
        // made every layout report BIG_ENDIAN in 2026-08-10.
        if let Some(name_slot) = name_slot {
            shared
                .mem
                .heap
                .set_field(obj, name_slot, Value::Object(None));
        }
        return Some(obj);
    }
    // JDK-ONLY-LAYOUT: converted (step 3) — this whole function is now
    // unreachable under `CompatibilityMode::JdkOnly`; the paragraphs below
    // describe what it still does in Compatible mode. It assumes slot 0 =
    // `byteSize`, slot 1 = `byteAlignment`.
    //
    // Every `class_name` reached here (`java/lang/foreign/ValueLayout$Of*`,
    // `java/lang/foreign/AddressLayout`) is an INTERFACE in the real JDK: it
    // has zero instance fields, and the concrete carrier is
    // `jdk/internal/foreign/layout/ValueLayouts$Of*Impl` (whose size/alignment
    // live on `AbstractLayout`, under different names and in an unspecified
    // order). The `.max(2)` above therefore invents two slots on an object of
    // an interface type and writes into them positionally — a fabricated
    // layout, not merely a mis-numbered one. This pairs with the
    // `has_clinit && != "java/lang/foreign/ValueLayout"` suppression above,
    // which stops the real `<clinit>` from ever running.
    //
    // Wave-2 requirement, DONE 2026-08-10: under `CompatibilityMode::JdkOnly`
    // the preseed is dropped entirely and the real `ValueLayout.<clinit>` runs
    // — both halves are gated at their own sites (`prepare_class`'s call below,
    // and the `has_clinit` suppression in `initialize_class_shared`). It was
    // NOT converted to named-field lookup, because there are no real fields to
    // name: it is a `CompatibilityClassRequested` violation, not a
    // slot-numbering bug, and the only honest fix for one of those is to stop
    // fabricating.
    //
    // The precondition the requirement named is met: `post_clinit_fixup`
    // backfills `jdk/internal/misc/UnsafeConstants` with real platform values,
    // so the real `<clinit>` no longer rebuilds ADDRESS through a zero
    // `ADDRESS_SIZE0`.
    shared.mem.heap.set_field(obj, 0, Value::Long(byte_size));
    shared
        .mem
        .heap
        .set_field(obj, 1, Value::Long(byte_alignment));
    Some(obj)
}

fn prepare_class_shared(shared: &SharedVm, class_id: ClassId) -> Result<(), VmError> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;

    // Gather static field info with read lock. For each static field, capture
    // the descriptor and (if present) the resolved ConstantValue: either a
    // primitive `Value` or a borrowed Utf8 string for ConstantValue=String.
    enum CvSeed {
        Primitive(Value),
        StringUtf8(String),
        /// `ConstantValue` String whose Utf8 holds lone surrogates — exact
        /// UTF-16 units (a Rust `String` cannot represent them).
        StringUtf16(Vec<u16>),
    }

    let (class_name, static_field_info): (String, Vec<(String, String, Option<CvSeed>)>) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id).ok_or_else(|| VmError::Internal {
            message: format!("prepare_class: class {class_id} not found"),
        })?;

        let mut info = Vec::new();
        for field in class.fields.iter() {
            if field.is_static() {
                let cv_index = field.constant_value_index();
                let seed =
                    cv_index.and_then(|cp_index| match class.constant_pool.get(cp_index)? {
                        ConstantPoolEntry::Integer(v) => Some(CvSeed::Primitive(Value::Int(*v))),
                        ConstantPoolEntry::Float(v) => Some(CvSeed::Primitive(Value::Float(*v))),
                        ConstantPoolEntry::Long(v) => Some(CvSeed::Primitive(Value::Long(*v))),
                        ConstantPoolEntry::Double(v) => Some(CvSeed::Primitive(Value::Double(*v))),
                        ConstantPoolEntry::StringReference { string_index } => {
                            if let Some(units) = class.constant_pool.get_utf8_wide(*string_index) {
                                Some(CvSeed::StringUtf16(units.to_vec()))
                            } else {
                                class
                                    .constant_pool
                                    .get_utf8(*string_index)
                                    .map(|s| CvSeed::StringUtf8(s.to_string()))
                            }
                        }
                        _ => None,
                    });
                info.push((field.name.to_string(), field.descriptor.to_string(), seed));
            }
        }
        (class.name.to_string(), info)
    };

    // Compiled `getstatic` needs a lock-free answer to "is this the class whose
    // statics the `System.out`/`err`/`in` bootstrap intercept services?" before
    // it may emit a direct load. Preparation is the earliest point the name is
    // known, and it always precedes initialization — which the inline path also
    // requires — so the id is always recorded before any inline decision about
    // this class can be taken. See `ClassRealm::system_class_id`.
    if class_name == "java/lang/System" {
        shared
            .classes
            .system_class_id
            .store(class_id.as_u32(), std::sync::atomic::Ordering::Relaxed);
    }

    // Allocate static field slots with default values, then overlay any
    // resolved ConstantValue. String constants are allocated through the
    // VM's interning string pool so multiple classes that name the same
    // literal share the same ObjectRef.
    let num_static = static_field_info.len();
    let mut statics = vec![Value::Int(0); num_static];

    for (static_idx, (field_name, descriptor, seed)) in static_field_info.iter().enumerate() {
        statics[static_idx] = default_value_for_descriptor(descriptor);

        match seed {
            Some(CvSeed::Primitive(v)) => statics[static_idx] = *v,
            Some(CvSeed::StringUtf8(text)) => {
                let str_ref =
                    super::vm_object::try_create_java_string(shared, text).ok_or_else(|| {
                        VmError::Runtime(RuntimeError::OutOfMemoryError {
                            message: format!(
                                "Java heap space (prepare_class {class_name}.{field_name} ConstantValue String)"
                            ),
                        })
                    })?;
                statics[static_idx] = Value::Object(Some(str_ref));
            }
            Some(CvSeed::StringUtf16(units)) => {
                let str_ref = super::vm_object::try_create_java_string_from_units(shared, units)
                    .ok_or_else(|| {
                        VmError::Runtime(RuntimeError::OutOfMemoryError {
                            message: format!(
                                "Java heap space (prepare_class {class_name}.{field_name} ConstantValue String)"
                            ),
                        })
                    })?;
                statics[static_idx] = Value::Object(Some(str_ref));
            }
            None => {}
        }

        // JDK-ONLY-LAYOUT (step 3): the preseed fabricates a two-slot instance
        // layout on an INTERFACE, so under `CompatibilityMode::JdkOnly` it is
        // skipped and the real `ValueLayout.<clinit>` runs instead (the
        // suppression in `initialize_class_shared` is lifted in the same mode).
        // Compatible mode is unchanged.
        if class_name == "java/lang/foreign/ValueLayout"
            && !shared.compatibility_mode().is_jdk_only()
        {
            if let Some((layout_class, byte_size, byte_alignment)) =
                value_layout_preseed(field_name)
            {
                if let Some(obj) =
                    make_prepared_value_layout(shared, layout_class, byte_size, byte_alignment)
                {
                    statics[static_idx] = Value::Object(Some(obj));
                }
            }
        }
    }

    // Publish to the lock-free index BEFORE inserting into the map, so a
    // concurrent reader either sees nothing (and takes the locked path, which
    // blocks on the write below) or sees a fully-populated block.
    let block = crate::vm::realms::class_realm::StaticsBlock::from_values(statics);
    shared.classes.statics_index.publish(class_id, &block);
    shared.classes.statics.write().insert(class_id, block);
    Ok(())
}

// ---------------------------------------------------------------------------
// Standalone helpers for class preparation
// ---------------------------------------------------------------------------

/// Return the default zero value for a field based on its JVM descriptor.
pub fn default_value_for_descriptor(descriptor: &str) -> Value {
    match descriptor.as_bytes().first() {
        Some(b'B' | b'C' | b'I' | b'S' | b'Z') => Value::Int(0),
        Some(b'J') => Value::Long(0),
        Some(b'F') => Value::Float(0.0),
        Some(b'D') => Value::Double(0.0),
        Some(b'L' | b'[') => Value::Object(None),
        _ => Value::Int(0),
    }
}

/// Default-value slots for a class's statics block, each typed by its static
/// field's **descriptor** rather than left at a blanket `Value::Int(0)`.
///
/// A statics slot holding a well-formed `Value` of the wrong WIDTH is invisible
/// from the interpreter (which widens an `Int` where a long is wanted) and
/// fatal from JIT-compiled code, which lowers `getstatic …:J` to a 64-bit load
/// at `FIELD_CELL_PAYLOAD64_OFFSET` and reads whatever sits beside an
/// `Int`-tagged cell. That is the defect behind the Spring Boot loader/zip
/// cluster, and `4972cd9c91` fixed it for exactly one writer
/// (`post_clinit_fixup`) — the hazard itself is a property of the slot, not of
/// that writer.
///
/// `StaticsBlock::new` fills every slot with `Value::Int(0)`, so a block NOT
/// built by `prepare_class` — which does call [`default_value_for_descriptor`]
/// per slot — starts out mistyped for every `J`/`D` static it holds.
/// `set_static_shared` builds one that way whenever a static is written before
/// its class is prepared. This is the shared helper that makes both paths
/// agree.
///
/// `total_len` is the block length the caller wants: historically the class's
/// TOTAL field count, while slot indices are the STATIC-field enumeration
/// order. The tail past `static_descriptors` is slack and keeps the zero fill.
pub fn typed_default_static_slots<S: AsRef<str>>(
    static_descriptors: &[S],
    total_len: usize,
) -> Vec<Value> {
    let mut slots = vec![Value::Int(0); total_len];
    for (slot, descriptor) in slots.iter_mut().zip(static_descriptors.iter()) {
        *slot = default_value_for_descriptor(descriptor.as_ref());
    }
    slots
}

/// Resolve a `ConstantValue` attribute index into a `Value`.
pub fn resolve_constant_value(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    index: u16,
) -> Option<Value> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    match cp.get(index)? {
        ConstantPoolEntry::Integer(v) => Some(Value::Int(*v)),
        ConstantPoolEntry::Float(v) => Some(Value::Float(*v)),
        ConstantPoolEntry::Long(v) => Some(Value::Long(*v)),
        ConstantPoolEntry::Double(v) => Some(Value::Double(*v)),
        ConstantPoolEntry::StringReference { .. } => None,
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ClassStoreHierarchy вЂ” adapter for the bytecode verifier
// ---------------------------------------------------------------------------

/// Adapter that implements [`ClassHierarchy`] using the VM's [`ClassStore`].
pub struct ClassStoreHierarchy<'a> {
    pub store: &'a ClassStore,
}

impl<'a> crate::classloading::vtype::ClassHierarchy for ClassStoreHierarchy<'a> {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        if child == parent {
            return true;
        }
        if parent == "java/lang/Object" {
            return true;
        }

        // Try the class store first
        let child_class = self.store.find_by_name(child);
        let parent_class = self.store.find_by_name(parent);

        if let (Some(cc), Some(pc)) = (child_class, parent_class) {
            return cc.is_subclass_of(pc.id, self.store);
        }

        // Fallback: walk the JDK superclass table for classes not yet loaded.
        // This is needed during verification when referenced classes haven't been
        // loaded/stubbed yet (e.g., IOException referenced in exception handler).
        let mut current = child;
        for _ in 0..20 {
            // depth limit to avoid infinite loops
            let sup = crate::classloading::jdk_superclass_lookup(current);
            if sup == parent {
                return true;
            }
            if sup == "java/lang/Object" {
                break;
            }
            current = sup;
        }

        // If either class is not loaded, be optimistic вЂ” the compiler guarantees
        // type safety, and the verifier can't prove otherwise without full class loading.
        // This handles: exception handler types, JAR classes not yet loaded, etc.
        if child_class.is_none() || parent_class.is_none() {
            return true;
        }

        false
    }

    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        let Some(child_class) = self.store.find_by_name(child) else {
            return false;
        };
        child_class
            .superclass
            .and_then(|super_id| self.store.get(super_id))
            .is_some_and(|super_class| super_class.name.as_ref() == parent)
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        if a == b {
            return a.to_string();
        }

        let mut a_chain: Vec<String> = Vec::new();
        if let Some(a_class) = self.store.find_by_name(a) {
            a_chain.push(a_class.name.to_string());
            let mut current = a_class.superclass;
            while let Some(super_id) = current {
                if let Some(super_class) = self.store.get(super_id) {
                    a_chain.push(super_class.name.to_string());
                    current = super_class.superclass;
                } else {
                    break;
                }
            }
        }

        if let Some(b_class) = self.store.find_by_name(b) {
            if a_chain.iter().any(|n| &**n == &*b_class.name) {
                return b_class.name.to_string();
            }
            let mut current = b_class.superclass;
            while let Some(super_id) = current {
                if let Some(super_class) = self.store.get(super_id) {
                    if a_chain.iter().any(|n| &**n == &*super_class.name) {
                        return super_class.name.to_string();
                    }
                    current = super_class.superclass;
                } else {
                    break;
                }
            }
        }

        crate::classloading::static_common_superclass_lookup(a, b)
    }

    fn is_interface(&self, name: &str) -> bool {
        self.store
            .find_by_name(name)
            .is_some_and(|c| c.is_interface())
    }
}

// ---------------------------------------------------------------------------
// Post-clinit fixup for swallowed <clinit> exceptions
// ---------------------------------------------------------------------------

/// Coerce a post-`<clinit>`-fixup value to the `Value` variant a field with
/// `descriptor` must hold, or `None` when the injected value cannot represent
/// that type at all.
///
/// The fixup call sites write plain integer literals; the field they land in
/// is whatever the JDK currently declares. When the two disagree the slot ends
/// up holding a well-formed `Value` of the WRONG width, which the interpreter
/// silently tolerates (its `getstatic` widens an `Int` where a long is wanted)
/// and JIT-compiled code does not (a `getstatic …:J` is a 64-bit load of the
/// slot). See the call site for the `Unsafe.ARRAY_*_BASE_OFFSET` case that
/// made `Arrays.equals(long[],long[])` return true for unequal arrays.
fn coerce_static_to_descriptor(descriptor: &str, value: Value) -> Option<Value> {
    let as_i64 = match value {
        Value::Int(i) => Some(i as i64),
        Value::Long(l) => Some(l),
        _ => None,
    };
    match descriptor {
        "J" => as_i64.map(Value::Long),
        "I" | "S" | "B" | "C" | "Z" => as_i64.map(|v| Value::Int(v as i32)),
        "F" => as_i64.map(|v| Value::Float(v as f32)),
        "D" => as_i64.map(|v| Value::Double(v as f64)),
        // Reference-typed field: only a reference may be written, and one is
        // already the right shape (nothing to widen).
        _ => match value {
            Value::Object(_) => Some(value),
            _ => None,
        },
    }
}

/// Report one post-`<clinit>` fixup arm's outcome at the level that outcome
/// deserves, instead of at a fixed `warn!`.
///
/// # 2026-09-01: why these stopped being warnings
///
/// Every one of these lines fired at `warn!` on the SUCCESS path, so a stock
/// `cratonvm Hello` — a one-line hello-world — printed four to six of them on
/// a boot where nothing had gone wrong (`UnsafeConstants populated (5/5)`,
/// `Unsafe ARRAY_*… populated (18/18)`, both BigInteger lines). A warning that
/// fires on every successful run is not a warning: it is the background a real
/// one has to be noticed against, and any tool reading this VM's stderr — CI,
/// a build script, a subprocess consumer, a test harness — sees it. That is
/// the same measurement that moved the descriptor-coercion guard behind
/// `cratonvm_types::compact_value::coercion_census`.
///
/// The distinction the old lines never drew is the one worth keeping. An arm
/// RUNNING is routine — that is what the arm is for, and the `<clinit>` that
/// made it necessary was already reported by the swallow path. An arm running
/// and coming up SHORT is not routine: a static this VM depends on is still
/// holding the JDK default, so the next reader of it gets a null or a zero
/// instead of the value the layout requires — silently, which is exactly the
/// class of damage `set_static_by_name`'s own descriptor refusal exists to
/// stop. So the complete case is demoted and the shortfall stays `warn!`, with
/// wording that says it is a shortfall rather than a status line.
///
/// # `info!`, not `debug!`
///
/// The workspace pins `tracing` with `release_max_level_info` (root
/// `Cargo.toml`), which compiles every `tracing::debug!` out of a release
/// build. Demoting to `debug!` would therefore not move these lines off the
/// default path, it would DELETE them from the binary that ships and that CI
/// runs — unrecoverable by any `RUST_LOG`. `info!` sits below `vm-cli`'s
/// WARN-only default filter (so a quiet boot stays quiet) and is still
/// reachable with `RUST_LOG=cratonvm_vm=info` in a release build, which is the
/// property that makes this a demotion rather than a deletion.
///
/// `already_ok` is the count that needed no repair because the slot already
/// held a real value — a SUCCESS, and nonzero only for the arms built on
/// `set_static_if_zero`. Counting it apart from `populated` is what lets the
/// shortfall test mean "a static could not be repaired" rather than "a static
/// did not need repairing"; without it the healthy end-state that helper's own
/// comment predicts (`0/19`, once the underlying ordering is fixed) would read
/// as a nineteen-field failure.
fn note_fixup_outcome(what: &str, populated: i64, already_ok: i64, expected: i64) {
    let missing = expected - populated - already_ok;
    if missing <= 0 {
        tracing::info!("Post-clinit fixup: {what} populated ({populated}/{expected})");
        return;
    }
    tracing::warn!(
        "Post-clinit fixup SHORTFALL: {what} — {missing} of {expected} static(s) could NOT be \
         repaired (populated={populated}, already-correct={already_ok}). Each unrepaired slot \
         still holds the JDK default (null/0), so the next read of it gets that instead of the \
         value this VM's layout requires, with no exception; any slot rejected on a descriptor \
         mismatch was named by its own refusal warning above."
    );
}

/// After swallowing a `<clinit>` failure, populate critical static fields
/// that downstream code unconditionally dereferences. Without this, swallowed
/// `<clinit>` failures leave static fields as null/0, causing NPEs in code
/// that depends on class initialization having completed.
fn post_clinit_fixup(shared: &SharedVm, class_id: ClassId, class_name: &str) {
    // Helper: find a static field index by name and set its value.
    //
    // CRITICAL: `resolve_field_ref` (vm/src/runtime/interpreter.rs) numbers
    // statics with a static-only counter (`static_idx`), not the position
    // in `cls.fields`.  `set_static_shared` reads/writes by that same
    // static-only index.  Earlier versions of this helper used
    // `cls.fields.iter().enumerate()` which counts BOTH statics and
    // instance fields — producing the wrong slot for any class with
    // interleaved static/instance declarations (e.g. BigDecimal puts
    // INFLATED/JLA between intVal and intCompact).  The mismatch silently
    // wrote `ZERO`/`ONE`/`TEN` into the wrong slots and the public statics
    // were read back as default-zero by the interpreter, surfacing as the
    // "0 0 OK" output and the cascade NPE on signum during BigDecimal
    // <clinit>.
    //
    // EQUALLY CRITICAL, and the same class of defect one level down: the
    // injected `Value`'s VARIANT must match the field's declared descriptor.
    // Every caller below writes an integer literal, and `Unsafe`'s nine
    // `ARRAY_*_BASE_OFFSET` fields are declared `J` (they became `long` in
    // JDK 25; the sibling `ARRAY_*_INDEX_SCALE` are still `I`), so they were
    // being handed a `Value::Int(16)`.
    //
    // The interpreter tolerated that — its `getstatic` reads the slot's
    // `Value` and widens an `Int` where a long is wanted — so nothing failed
    // for the first eighteen months. JIT-compiled code does not: a
    // `getstatic …:J` lowers to a 64-bit load of the slot, which over an
    // `Int`-tagged slot reads adjacent memory. Measured: compiled
    // `jdk.internal.util.ArraysSupport.mismatch(int[],int[],int)` called
    // `vectorizedMismatch(a, ARRAY_INT_BASE_OFFSET, …)` with an offset of
    // 0x7ff700000000 instead of 16, the native's range check then failed, it
    // returned -1 ("no mismatch"), and `Arrays.mismatch`/`Arrays.equals`
    // reported two DIFFERENT int[]/long[] arrays as equal. See
    // the internal record
    // `batchtest-jit-duplicate-batch-insert-unique-violation-20260804.md`.
    //
    // So coerce here rather than trusting ~30 call sites to keep tracking the
    // JDK's field types: the descriptor is right there next to the name, and
    // a fixup that writes the wrong width is worse than no fixup at all
    // (a swallowed `<clinit>` at least leaves a well-typed zero).
    //
    // A second, independent symptom of the same slot, found in parallel on the
    // Spring Boot loader shard: `java.util.zip.ZipUtils.get16` is
    // `getShortUnaligned(b, off + ARRAY_BYTE_BASE_OFFSET)`, so compiled ZIP
    // central-directory parses addressed 8 bytes before the array data and the
    // extra-field walk silently found nothing —
    // `ZipContentTests.entryWithEpochTimeOfZeroShouldNotFail` read the DOS
    // fallback 1980-01-01 instead of the extended timestamp's 1970-01-01. It
    // passes cold and fails once the method is hot, which is the tell. See
    // `spring-boot-loader-residual-20260723-FIXED.md`.
    let set_static_by_name = |field_name: &str, value: Value| {
        let cm = shared.classes.class_manager.read();
        if let Some(cls) = cm.get_class(class_id) {
            let mut static_idx = 0usize;
            for f in &cls.fields {
                if f.is_static() {
                    if &*f.name == field_name {
                        let descriptor = Arc::clone(&f.descriptor);
                        drop(cm);
                        let Some(typed) = coerce_static_to_descriptor(&descriptor, value) else {
                            // Refuse rather than write a mistyped slot, and say
                            // so: the per-class `n/total` warnings below then
                            // report a shortfall instead of a silent success.
                            tracing::warn!(
                                "Post-clinit fixup: refusing to write {class_name}.{field_name} \
                                 — injected value {value:?} does not fit declared type \
                                 `{descriptor}`"
                            );
                            return false;
                        };
                        super::vm_object::set_static_shared(shared, class_id, static_idx, typed);
                        return true;
                    }
                    static_idx += 1;
                }
            }
        }
        false
    };

    // Statics `set_static_if_zero` left alone because they already held a
    // non-zero value. See `note_fixup_outcome` for why this cannot be folded
    // into the per-arm repair count.
    let already_populated = std::cell::Cell::new(0i32);

    // Write ONLY a slot that still holds zero.
    //
    // `set_static_by_name` below writes unconditionally, so a count of
    // repairs made with it reports "fields found", not "fields that were
    // broken" -- and it would overwrite a correct value if the underlying
    // ordering were ever fixed. Reading first makes the per-arm `n/total`
    // warning a measurement: `19/19` means nineteen really were zero, and a
    // later `0/19` means the arm has become dead weight and can go.
    let set_static_if_zero = |field_name: &str, value: Value| {
        // Declining because the slot already held a real value is a SUCCESS
        // and must not be reported as a shortfall; declining because the field
        // does not exist is a defect. A bare `n/total` cannot tell them apart,
        // so the first outcome is counted here and handed to
        // `note_fixup_outcome` separately.
        let idx = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(class_id).and_then(|cls| {
                let mut static_idx = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        if &*f.name == field_name {
                            return Some(static_idx);
                        }
                        static_idx += 1;
                    }
                }
                None
            })
        };
        let Some(static_idx) = idx else {
            return false;
        };
        let current = super::vm_object::get_static_shared(shared, class_id, static_idx);
        let is_zero = matches!(
            current,
            Value::Int(0) | Value::Long(0) | Value::Object(None)
        );
        if !is_zero {
            already_populated.set(already_populated.get() + 1);
            return false;
        }
        set_static_by_name(field_name, value)
    };


    match class_name {
        "jdk/internal/misc/UnsafeConstants" => {
            // Inject the platform constants HotSpot would set natively at
            // bootstrap (see the success-path call site in `init_class`). All
            // five fields default to 0/false from the real `<clinit>`; we patch
            // them to the x86-64 / Windows values. Booleans use Value::Int(0|1).
            //   ADDRESS_SIZE0            = 8     (64-bit native pointer)
            //   PAGE_SIZE                = 4096  (Windows/x86-64 base page)
            //   BIG_ENDIAN               = false (x86-64 is little-endian)
            //   UNALIGNED_ACCESS         = true  (x86 permits unaligned access)
            //   DATA_CACHE_LINE_FLUSH_SIZE = 0   (CLFLUSH writeback advertised
            //                                     disabled: isWritebackEnabled()
            //                                     == (size != 0) stays false)
            let mut n = 0;
            n += set_static_by_name("ADDRESS_SIZE0", Value::Int(8)) as i32;
            n += set_static_by_name("PAGE_SIZE", Value::Int(4096)) as i32;
            n += set_static_by_name("BIG_ENDIAN", Value::Int(0)) as i32;
            n += set_static_by_name("UNALIGNED_ACCESS", Value::Int(1)) as i32;
            n += set_static_by_name("DATA_CACHE_LINE_FLUSH_SIZE", Value::Int(0)) as i32;
            note_fixup_outcome("UnsafeConstants", i64::from(n), 0, 5);
        }
        "jdk/internal/misc/Unsafe" => {
            // ES-FAIL-FAMILY-20260710: `Unsafe.<clinit>` computes each
            // `ARRAY_*_BASE_OFFSET`/`ARRAY_*_INDEX_SCALE` static by calling
            // `theUnsafe.arrayBaseOffset(...)`/`arrayIndexScale(...)`
            // (which call the private `arrayBaseOffset0`/`arrayIndexScale0`
            // natives) during real-JDK bootstrap, before this VM's native
            // registry has those methods wired up (they are registered in a
            // later "Session 10" pass). The `<clinit>` itself does not throw
            // -- the unregistered native calls silently return the zero
            // value for their return type -- so every one of these 18
            // `static final` fields is permanently latched at 0 for the
            // rest of the process.
            //
            // Any code that follows the documented Unsafe protocol
            // (`offset = ARRAY_<T>_BASE_OFFSET + index`) then hands
            // `getIntUnaligned`/`getShortUnaligned`/etc. an offset that is
            // short by 16, so those reads land on the wrong bytes. This is
            // silent (no exception) and byte-for-byte deterministic, so the
            // corruption is not visible until something decodes the
            // misread value -- e.g. `java.util.zip.ZipUtils.get32/get16`
            // (used by `ZipInputStream.getNextEntry()`'s LOC-header parser),
            // which is how `Build$CurrentHolder.findCurrent()` was observed
            // to see `JarInputStream.getManifest()` silently return null
            // for the real `elasticsearch-<version>.jar`.
            //
            // Values match `native_unsafe_array_base_offset`/
            // `array_index_scale_for_name` in native-builtins/src/lib.rs --
            // CratonVM's own uniform "16-byte header, no compressed oops"
            // convention, not a per-type HotSpot table.
            let mut n = 0;
            for name in [
                "ARRAY_BOOLEAN_BASE_OFFSET",
                "ARRAY_BYTE_BASE_OFFSET",
                "ARRAY_SHORT_BASE_OFFSET",
                "ARRAY_CHAR_BASE_OFFSET",
                "ARRAY_INT_BASE_OFFSET",
                "ARRAY_LONG_BASE_OFFSET",
                "ARRAY_FLOAT_BASE_OFFSET",
                "ARRAY_DOUBLE_BASE_OFFSET",
                "ARRAY_OBJECT_BASE_OFFSET",
            ] {
                n += set_static_by_name(name, Value::Int(16)) as i32;
            }
            for (name, scale) in [
                ("ARRAY_BOOLEAN_INDEX_SCALE", 1),
                ("ARRAY_BYTE_INDEX_SCALE", 1),
                ("ARRAY_SHORT_INDEX_SCALE", 2),
                ("ARRAY_CHAR_INDEX_SCALE", 2),
                ("ARRAY_INT_INDEX_SCALE", 4),
                ("ARRAY_LONG_INDEX_SCALE", 8),
                ("ARRAY_FLOAT_INDEX_SCALE", 4),
                ("ARRAY_DOUBLE_INDEX_SCALE", 8),
                ("ARRAY_OBJECT_INDEX_SCALE", 8),
            ] {
                n += set_static_by_name(name, Value::Int(scale)) as i32;
            }
            note_fixup_outcome(
                "Unsafe ARRAY_*_BASE_OFFSET/INDEX_SCALE",
                i64::from(n),
                0,
                18,
            );
        }
        "sun/misc/Unsafe" => {
            // L1-R5-20260830: the LEGACY spelling's own 18 array constants and
            // `ADDRESS_SIZE` latch at 0 for exactly the reason the
            // `jdk/internal/misc/Unsafe` arm above documents
            // (ES-FAIL-FAMILY-20260710): `<clinit>` computes them through
            // natives that are not registered this early in boot, and an
            // unregistered native silently returns its return type's zero
            // rather than throwing. `sun.misc.Unsafe.<clinit>` copies them
            // from `jdk/internal/misc/Unsafe` at bci 43..157 and reads
            // `addressSize()` at bci 166 -- all before that sibling arm has
            // run -- so it copies zeros.
            //
            // MEASURED on JDK 25, --jdk-only, diffed against HotSpot
            // 25.0.4+7: `ARRAY_OBJECT_INDEX_SCALE non-zero` and
            // `ADDRESS_SIZE non-zero` were both FALSE here and both true on
            // HotSpot. Same silent-corruption shape: a library following the
            // documented `ARRAY_<T>_BASE_OFFSET + index` protocol through the
            // legacy spelling gets an offset short by 16, with no exception.
            //
            // Values are the sibling arm's, unchanged.
            let mut legacy = 0;
            for name in [
                "ARRAY_BOOLEAN_BASE_OFFSET",
                "ARRAY_BYTE_BASE_OFFSET",
                "ARRAY_SHORT_BASE_OFFSET",
                "ARRAY_CHAR_BASE_OFFSET",
                "ARRAY_INT_BASE_OFFSET",
                "ARRAY_LONG_BASE_OFFSET",
                "ARRAY_FLOAT_BASE_OFFSET",
                "ARRAY_DOUBLE_BASE_OFFSET",
                "ARRAY_OBJECT_BASE_OFFSET",
            ] {
                legacy += set_static_if_zero(name, Value::Int(16)) as i32;
            }
            for (name, scale) in [
                ("ARRAY_BOOLEAN_INDEX_SCALE", 1),
                ("ARRAY_BYTE_INDEX_SCALE", 1),
                ("ARRAY_SHORT_INDEX_SCALE", 2),
                ("ARRAY_CHAR_INDEX_SCALE", 2),
                ("ARRAY_INT_INDEX_SCALE", 4),
                ("ARRAY_LONG_INDEX_SCALE", 8),
                ("ARRAY_FLOAT_INDEX_SCALE", 4),
                ("ARRAY_DOUBLE_INDEX_SCALE", 8),
                ("ARRAY_OBJECT_INDEX_SCALE", 8),
            ] {
                legacy += set_static_if_zero(name, Value::Int(scale)) as i32;
            }
            // `size_of::<usize>()`, matching `native_unsafe_address_size` and
            // the `ADDRESS_SIZE0` backfill earlier in this file.
            legacy += set_static_if_zero("ADDRESS_SIZE", Value::Int(8)) as i32;
            // Snapshot so the latch report below can subtract it: both blocks
            // in this arm write through the same `set_static_if_zero`.
            let already_arrays = already_populated.get();
            note_fixup_outcome(
                "sun.misc.Unsafe ARRAY_*/ADDRESS_SIZE",
                i64::from(legacy),
                i64::from(already_arrays),
                19,
            );

            // The memory-access warning latch (`<clinit>` bci 185/195). Same
            // root cause, and the last of L1 R5: `staticFieldBase` and
            // `staticFieldOffset` were unregistered when `<clinit>` called
            // them, so they returned `null` and `0L` without throwing, and
            // every legacy Unsafe access since has run its latch against an
            // offset no side table can resolve -- landing in a private store
            // where a CAS reports success having written nowhere a reader can
            // see (513+ times in one H2 full-text vector).
            //
            // Mint through the SAME call the `<clinit>` would have made, so
            // the offset is REGISTERED and not merely numbered: an
            // unregistered offset moves the defect instead of fixing it.
            let warned_idx = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id).and_then(|cls| {
                    let mut static_idx = 0usize;
                    for f in &cls.fields {
                        if f.is_static() {
                            if &*f.name == "memoryAccessWarned" {
                                return Some(static_idx);
                            }
                            static_idx += 1;
                        }
                    }
                    None
                })
            };
            if let Some(widx) = warned_idx {
                let offset = cratonvm_native_builtins::register_static_field_offset(
                    "sun/misc/Unsafe",
                    "memoryAccessWarned",
                    class_id,
                    widx,
                );
                // The class mirror is what `native_unsafe_static_field_base`
                // answers for a static field on this VM, and `StaticBaseProbe`
                // proved that shape round-trips byte-identically to HotSpot.
                let mirror = super::get_or_create_class_mirror(shared, class_id);
                let mut latch = 0;
                latch += set_static_if_zero("MEMORY_ACCESS_WARNED_OFFSET", Value::Long(offset))
                    as i32;
                latch += set_static_if_zero(
                    "MEMORY_ACCESS_WARNED_BASE",
                    Value::Object(Some(mirror)),
                ) as i32;
                note_fixup_outcome(
                    "sun.misc.Unsafe memory-access latch",
                    i64::from(latch),
                    i64::from(already_populated.get() - already_arrays),
                    2,
                );
            } else {
                tracing::warn!(
                    "Post-clinit fixup: sun.misc.Unsafe memoryAccessWarned not found —                      latch NOT repaired"
                );
            }

            // JDK 25's `Unsafe.<clinit>` stores the result of
            // `MemoryAccessOption.value()` in MEMORY_ACCESS_OPTION. In a
            // real-JDK CratonVM boot, that particular static store can remain
            // null even though both the configured property and the enum
            // constants are already live. Every legacy Unsafe memory access
            // then enters beforeMemoryAccessSlow and dereferences null via
            // `MEMORY_ACCESS_OPTION.ordinal()`.
            //
            // Preserve the JDK's option semantics rather than always forcing
            // ALLOW: the VM default is `allow`, while an explicit user
            // override may request warn/debug/deny. Only repair a missing
            // value so a correctly initialized future implementation remains
            // authoritative.
            let policy_field = match shared
                .system_properties
                .read()
                .get("sun.misc.unsafe.memory.access")
                .map(String::as_str)
            {
                Some("allow") => "ALLOW",
                Some("debug") => "DEBUG",
                Some("deny") => "DENY",
                Some("warn") | None => "WARN",
                Some(_) => "WARN",
            };
            let unsafe_option_slot = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id).and_then(|unsafe_class| {
                    let mut static_idx = 0usize;
                    for field in &unsafe_class.fields {
                        if field.is_static() {
                            if &*field.name == "MEMORY_ACCESS_OPTION" {
                                return Some(static_idx);
                            }
                            static_idx += 1;
                        }
                    }
                    None
                })
            };
            // KEYCLOAK-MEMACCESS-FIELDINDEX-20260714: the manually-recomputed
            // `static_idx` above uses the same static-only, declaration-order
            // convention as `resolve_field_ref`/`set_static_by_name`, so it is
            // NOT the bug (verified against both call sites). But under the
            // real Keycloak/Infinispan boot path (unlike the isolated repair
            // probes), `sun/misc/Unsafe`'s statics vec can be lazily
            // pre-sized/touched by an unrelated earlier static write before
            // MEMORY_ACCESS_OPTION's own `<clinit>` store runs, and/or this
            // slot can otherwise hold a stale non-null `Object` left over from
            // a different code path than the enum constant this repair
            // expects. Trusting "slot is non-null" alone as "already
            // initialized" is therefore a false-positive trap: it forces
            // `repaired=false` and leaves the true null in place, reproducing
            // the exact NPE this fixup exists to prevent. Require the slot to
            // actually hold an instance of `MemoryAccessOption` (not just any
            // non-null object) before treating it as already initialized.
            // EVIDENCE (2026-07-14, isolated reflection-only Unsafe probe on
            // Azure host): the already-initialized check below correctly read
            // `Value::Int(0)` here (genuinely uninitialized, not a false
            // positive) while `find_class_by_name` returned `None` for
            // `sun/misc/Unsafe$MemoryAccessOption`. The success path now
            // loads and initializes that enum after finalizing Unsafe, before
            // this lookup-only repair pass runs. See docs/internal once the
            // regression is closed.
            let enum_class_id = shared
                .classes
                .class_manager
                .read()
                .find_bootstrap_class_by_name("sun/misc/Unsafe$MemoryAccessOption");
            let already_initialized = unsafe_option_slot.is_some_and(|static_idx| {
                match super::vm_object::get_static_shared(shared, class_id, static_idx) {
                    Value::Object(Some(obj)) => {
                        let obj_class_id = shared.mem.heap.class_id_of(obj);
                        let is_option = enum_class_id.is_some_and(|eid| obj_class_id == eid);
                        if !is_option {
                            tracing::warn!(
                                "Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION slot \
                                 held a non-null object of class_id={obj_class_id:?} (expected \
                                 MemoryAccessOption class_id={enum_class_id:?}) — treating as \
                                 NOT already initialized"
                            );
                        }
                        is_option
                    }
                    _ => false,
                }
            });
            let enum_slot = {
                let cm = shared.classes.class_manager.read();
                enum_class_id
                    .and_then(|enum_class_id| cm.get_class(enum_class_id))
                    .and_then(|enum_class| {
                        let mut static_idx = 0usize;
                        for field in &enum_class.fields {
                            if field.is_static() {
                                if &*field.name == policy_field {
                                    return Some((enum_class.id, static_idx));
                                }
                                static_idx += 1;
                            }
                        }
                        None
                    })
            };
            let repaired = if already_initialized {
                false
            } else if let Some((enum_class_id, static_idx)) = enum_slot {
                match super::vm_object::get_static_shared(shared, enum_class_id, static_idx) {
                    Value::Object(Some(option)) => {
                        set_static_by_name("MEMORY_ACCESS_OPTION", Value::Object(Some(option)))
                    }
                    other => {
                        tracing::warn!(
                            "Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION repair \
                             found enum_slot but its value was {other:?}, not \
                             Value::Object(Some(_)) — repair skipped"
                        );
                        false
                    }
                }
            } else {
                tracing::warn!(
                    "Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION repair could not \
                     resolve enum_slot (unsafe_option_slot={unsafe_option_slot:?} \
                     enum_class_id={enum_class_id:?} policy_field={policy_field}) — repair \
                     skipped"
                );
                false
            };
            // Routine either way: `repaired=false` is the healthy answer when
            // the slot already held a real `MemoryAccessOption`, and every path
            // that failed to repair a genuinely-empty slot has already warned
            // for itself above.
            tracing::info!(
                "Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION policy={policy_field} repaired={repaired}"
            );
        }
        "java/io/File" => {
            // See the success-path call site (`init_class`) for the full
            // root-cause writeup: `UnixFileSystem`'s constructor reads
            // `System.getProperties()`'s real `map` (`ConcurrentHashMap`)
            // field, which is null on the synthetic system-properties
            // singleton, so the read partway through `<clinit>` NPEs and
            // gets swallowed — leaving `File.FS` null and whichever separator
            // statics were assigned before the throw correct while the rest
            // stay at their zero-init default. Backfill deterministically.
            let sep_char = std::path::MAIN_SEPARATOR;
            let path_sep_char = if cfg!(windows) { ';' } else { ':' };
            let sep_str = super::vm_object::create_java_string(shared, &sep_char.to_string());
            let path_sep_str =
                super::vm_object::create_java_string(shared, &path_sep_char.to_string());
            let set_instance_by_name =
                |obj: ObjectRef, obj_class_id: ClassId, field_name: &str, value: Value| {
                    let cm = shared.classes.class_manager.read();
                    if let Some(cls) = cm.get_class(obj_class_id) {
                        let mut instance_idx = 0usize;
                        for f in &cls.fields {
                            if !f.is_static() {
                                if &*f.name == field_name {
                                    drop(cm);
                                    shared.mem.heap.set_field(obj, instance_idx, value);
                                    return true;
                                }
                                instance_idx += 1;
                            }
                        }
                    }
                    false
                };

            let mut n = 0;
            #[cfg(windows)]
            let fs_class_name = "java/io/WinNTFileSystem";
            #[cfg(not(windows))]
            let fs_class_name = "java/io/UnixFileSystem";
            let fs_class_id = shared.load_class_concurrent(fs_class_name).ok();
            let fs_class = fs_class_id.and_then(|id| {
                let cm = shared.classes.class_manager.read();
                cm.get_class(id).map(|cls| {
                    let fields = cls.fields.iter().filter(|f| !f.is_static()).count();
                    (id, fields)
                })
            });
            if let Some((fs_class_id, fs_fields)) = fs_class {
                if let Some(fs_obj) = shared.mem.heap.try_alloc_object(fs_class_id, fs_fields) {
                    let (java_home, user_dir) = {
                        let props = shared.system_properties.read();
                        (
                            props.get("java.home").cloned().unwrap_or_default(),
                            props
                                .get("user.dir")
                                .cloned()
                                .or_else(|| {
                                    std::env::current_dir()
                                        .ok()
                                        .map(|p| p.to_string_lossy().into_owned())
                                })
                                .unwrap_or_else(|| ".".to_string()),
                        )
                    };
                    let java_home_str = super::vm_object::create_java_string(shared, &java_home);
                    let user_dir_str = super::vm_object::create_java_string(shared, &user_dir);
                    let alt_sep_char = if sep_char == '\\' { '/' } else { '\\' };

                    let _ = set_instance_by_name(
                        fs_obj,
                        fs_class_id,
                        "slash",
                        Value::Int(sep_char as i32),
                    );
                    let _ = set_instance_by_name(
                        fs_obj,
                        fs_class_id,
                        "colon",
                        Value::Int(path_sep_char as i32),
                    );
                    let _ = set_instance_by_name(
                        fs_obj,
                        fs_class_id,
                        "semicolon",
                        Value::Int(path_sep_char as i32),
                    );
                    let _ = set_instance_by_name(
                        fs_obj,
                        fs_class_id,
                        "altSlash",
                        Value::Int(alt_sep_char as i32),
                    );
                    let _ = set_instance_by_name(
                        fs_obj,
                        fs_class_id,
                        "javaHome",
                        Value::Object(Some(java_home_str)),
                    );
                    let _ = set_instance_by_name(
                        fs_obj,
                        fs_class_id,
                        "userDir",
                        Value::Object(Some(user_dir_str)),
                    );
                    n += set_static_by_name("FS", Value::Object(Some(fs_obj))) as i32;
                }
            }
            n += set_static_by_name("separatorChar", Value::Int(sep_char as i32)) as i32;
            n += set_static_by_name("separator", Value::Object(Some(sep_str))) as i32;
            n += set_static_by_name("pathSeparatorChar", Value::Int(path_sep_char as i32)) as i32;
            n += set_static_by_name("pathSeparator", Value::Object(Some(path_sep_str))) as i32;
            note_fixup_outcome("File fs/separator/pathSeparator", i64::from(n), 0, 5);
        }
        // (Removed) "org/jboss/modules/Module" arm.
        //
        // Earlier this arm unconditionally overwrote `BOOT_MODULE_LOADER`
        // (and conditionally backfilled `systemPaths`/`systemPackages`)
        // with synthetic empty objects after a swallowed `<clinit>`.
        // Instrumentation (see 2026-05-19 diag run) confirmed that on the
        // current interpreter Module.<clinit> succeeds end-to-end and every
        // one of those statics is populated with a real heap object by the
        // JDK bytecode itself, so the synthetic backfill was both
        // unnecessary and actively harmful (it clobbered the real
        // `AtomicReference` published by `<clinit>`). Removed per the
        // synthetic-stubs policy. The B6 silent-swallow path that calls
        // this function is still in place — it is just a no-op for Module.
        "java/util/logging/LogManager" => {
            // LogManager.manager must be non-null for getLogManager()
            if let Some(mgr) = shared.mem.heap.try_alloc_object(class_id, 4) {
                if set_static_by_name("manager", Value::Object(Some(mgr))) {
                    tracing::info!("Post-clinit fixup: LogManager.manager populated");
                }
            }
        }
        "jdk/internal/icu/text/NormalizerBase$NFCModeImpl"
        | "jdk/internal/icu/text/NormalizerBase$NFDModeImpl"
        | "jdk/internal/icu/text/NormalizerBase$NFKCModeImpl"
        | "jdk/internal/icu/text/NormalizerBase$NFKDModeImpl"
        | "jdk/internal/icu/text/NormalizerBase$NFKC32ModeImpl" => {
            // ICU Normalizer data loading fails (resource not exposed via
            // getResourceAsStream in our classloader).  Each <Form>ModeImpl
            // holds a static INSTANCE:ModeImpl whose `normalizer2` field is
            // read by NormalizerBase$<Form>Mode.getNormalizer2().  If INSTANCE
            // is null, downstream getfield throws NPE.  Populate it with a
            // ModeImpl wrapping Norm2AllModes$NoopNormalizer2 вЂ” a concrete
            // Normalizer2 subclass present in the JDK that passes strings
            // through unchanged.  KC16 bootstrap never actually normalizes
            // real Unicode, so the pass-through behavior is sufficient.
            let mode_impl_name = "jdk/internal/icu/text/NormalizerBase$ModeImpl";
            let noop_name = "jdk/internal/icu/impl/Norm2AllModes$NoopNormalizer2";
            let (mode_impl_id, noop_id) = {
                let cm = shared.classes.class_manager.read();
                (
                    cm.find_bootstrap_class_by_name(mode_impl_name),
                    cm.find_bootstrap_class_by_name(noop_name),
                )
            };
            if let (Some(mid), Some(nid)) = (mode_impl_id, noop_id) {
                // NoopNormalizer2 has no instance fields.
                if let Some(noop_obj) = shared.mem.heap.try_alloc_object(nid, 0) {
                    // ModeImpl has 1 instance field: normalizer2:Normalizer2.
                    // JDK-ONLY-LAYOUT: safe — verified against JDK 25,
                    // `NormalizerBase$ModeImpl` is `final` and declares exactly
                    // one instance field (`private final Normalizer2
                    // normalizer2`) with `java/lang/Object` as its superclass,
                    // so slot 0 is unambiguous whatever the declaration order.
                    // Note this fixup itself is a compatibility substitution
                    // (it stands in for a failed resource load) and is in scope
                    // for the wave-2 `CompatibilityClassRequested` sweep even
                    // though its slot arithmetic is correct.
                    if let Some(mode_impl) = shared.mem.heap.try_alloc_object(mid, 1) {
                        shared
                            .mem
                            .heap
                            .set_field(mode_impl, 0, Value::Object(Some(noop_obj)));
                        if set_static_by_name("INSTANCE", Value::Object(Some(mode_impl))) {
                            tracing::info!(
                                class = %class_name,
                                "Post-clinit fixup: {}.INSTANCE populated with NoopNormalizer2 ModeImpl",
                                class_name
                            );
                        }
                    }
                }
            } else {
                tracing::warn!(
                    class = %class_name,
                    "Post-clinit fixup: cannot populate INSTANCE вЂ” ModeImpl or NoopNormalizer2 not loaded"
                );
            }
        }
        "java/lang/invoke/VarHandleInts$Array"
        | "java/lang/invoke/VarHandleLongs$Array"
        | "java/lang/invoke/VarHandleShorts$Array"
        | "java/lang/invoke/VarHandleBytes$Array"
        | "java/lang/invoke/VarHandleBooleans$Array"
        | "java/lang/invoke/VarHandleChars$Array"
        | "java/lang/invoke/VarHandleFloats$Array"
        | "java/lang/invoke/VarHandleDoubles$Array"
        | "java/lang/invoke/VarHandleReferences$Array" => {
            // The <clinit> of each VarHandle*$Array constructs a VarForm
            // via `new VarForm(Class, Class, Class, Class[])`, which
            // transitively calls MethodType.methodType / MethodTypeForm.canonicalize.
            // If any of those infrastructure classes fails or produces null,
            // the <clinit> NPEs and the static `FORM` field is left null.
            // Downstream code (e.g. Jackson's VarHandle fallback, H2's
            // DbException.<clinit>) reads FORM or allocates instances whose
            // <init>(II) path dereferences FORM, and NPEs.
            //
            // Populate FORM with a minimally-initialized VarForm stub.  The
            // VarForm instance fields (implClass, methodType_table,
            // memberName_table, methodType_V_table) are left as defaults вЂ”
            // they are consulted only by full VarHandle invocation, which
            // we intercept natively in lang_invoke.rs anyway.  Downstream
            // code only needs FORM to be non-null so that ordinary
            // getstatic + constructor chains proceed.
            let varform_name = "java/lang/invoke/VarForm";
            let varform_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_bootstrap_class_by_name(varform_name)
            };
            if let Some(vfid) = varform_id {
                // VarForm has 4 instance fields (see javap of VarForm).
                if let Some(vf_obj) = shared.mem.heap.try_alloc_object(vfid, 4) {
                    if set_static_by_name("FORM", Value::Object(Some(vf_obj))) {
                        tracing::info!(
                            class = %class_name,
                            "Post-clinit fixup: {}.FORM populated with stub VarForm",
                            class_name
                        );
                    }
                }
            } else {
                tracing::warn!(
                    class = %class_name,
                    "Post-clinit fixup: cannot populate FORM вЂ” VarForm class not loaded"
                );
            }
        }
        "io/quarkus/bootstrap/logging/InitialConfigurator" => {
            // DELAYED_HANDLER must be non-null вЂ” QuarkusEntryPoint.main() reads it.
            // Allocate a synthetic QuarkusDelayedHandler.
            let handler_class_name = "io/quarkus/bootstrap/logging/QuarkusDelayedHandler";
            let handler_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_class_by_name_for_class(handler_class_name, class_id)
            };
            if let Some(hid) = handler_id {
                if let Some(handler) = shared.mem.heap.try_alloc_object(hid, 4) {
                    if set_static_by_name("DELAYED_HANDLER", Value::Object(Some(handler))) {
                        tracing::info!(
                            "Post-clinit fixup: InitialConfigurator.DELAYED_HANDLER populated"
                        );
                    }
                }
            } else {
                // Class not yet loaded вЂ” allocate a generic Handler stub
                if let Some(handler) = shared.mem.heap.try_alloc_object(class_id, 4) {
                    if set_static_by_name("DELAYED_HANDLER", Value::Object(Some(handler))) {
                        tracing::info!("Post-clinit fixup: InitialConfigurator.DELAYED_HANDLER populated (generic)");
                    }
                }
            }
        }
        // T19.H4: JBoss Modules boot holder.  DefaultBootModuleLoaderHolder's
        // real <clinit> constructs a real ModuleLoader backed by -mp; our B6
        // path swallows the failure (WeakReference / MBean side-effects).
        // Populate INSTANCE with a synthetic LocalModuleLoader whose
        // loadModule native (registered by native-builtins) walks the -mp
        // filesystem at call time.  Without this fix, Main.main NPEs with
        // "Cannot invoke loadModule on null".
        "org/jboss/modules/DefaultBootModuleLoaderHolder" => {
            let loader_class_name = "org/jboss/modules/LocalModuleLoader";
            let loader_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_class_by_name_for_class(loader_class_name, class_id)
            };
            let target_id = loader_id.unwrap_or(class_id);
            if let Some(loader_obj) = shared.mem.heap.try_alloc_object(target_id, 1) {
                if set_static_by_name("INSTANCE", Value::Object(Some(loader_obj))) {
                    tracing::info!(
                        "Post-clinit fixup: DefaultBootModuleLoaderHolder.INSTANCE populated with synthetic LocalModuleLoader"
                    );
                }
            }
        }
        // KC16 RKC16N.14 / RBIGDEC.1 — `java/math/BigInteger.<clinit>` can fail
        // in real-JDK mode and leave ZERO/ONE/TWO/TEN/NEGATIVE_ONE null. We
        // populate those statics with concrete instances using the real-JDK
        // layout (slot 0 = signum:I, slot 1 = mag:[I).  The native handlers
        // in `native-builtins/src/lib.rs` (`bi_read` / `native_bi_signum`)
        // now read this layout directly via `resolve_field_index`, so no
        // descriptor-cache poisoning or synthetic-string overlay is needed.
        "java/math/BigInteger" => {
            // Locate signum + mag instance-field indices in the JDK layout.
            // `num_fields` must include inherited slots (Number adds none here,
            // but we honour the layout to be safe) — pass the full count to
            // `try_alloc_object` or downstream `getfield` reads land beyond
            // the allocated slot bound.
            let (signum_idx, mag_idx, num_fields) = {
                let cm = shared.classes.class_manager.read();
                if let Some(cls) = cm.get_class(class_id) {
                    let mut sig = None;
                    let mut mag = None;
                    let mut instance_offset = 0usize;
                    for f in cls.fields.iter() {
                        if f.is_static() {
                            continue;
                        }
                        match &*f.name {
                            "signum" => sig = Some(cls.first_field_index + instance_offset),
                            "mag" => mag = Some(cls.first_field_index + instance_offset),
                            _ => {}
                        }
                        instance_offset += 1;
                    }
                    (sig, mag, cls.num_total_fields)
                } else {
                    (None, None, 0)
                }
            };
            if let (Some(sig_i), Some(mag_i)) = (signum_idx, mag_idx) {
                // Look up an existing static; returns Some(ref) if non-null.
                let lookup_existing = |name: &str| -> Option<crate::types::ObjectRef> {
                    let cm = shared.classes.class_manager.read();
                    let cls = cm.get_class(class_id)?;
                    let mut static_idx = 0usize;
                    for f in &cls.fields {
                        if f.is_static() {
                            if &*f.name == name {
                                drop(cm);
                                match super::vm_object::get_static_shared(
                                    shared, class_id, static_idx,
                                ) {
                                    Value::Object(Some(o)) => return Some(o),
                                    _ => return None,
                                }
                            }
                            static_idx += 1;
                        }
                    }
                    None
                };
                // Build or patch a BigInteger constant.  If the static slot
                // already holds a real-JDK-allocated BigInteger (success-path
                // post_clinit_fixup hook), reuse it but rewrite signum/mag
                // to be sure they hold the canonical values — otherwise
                // allocate a fresh instance with the real-JDK layout.
                let make_or_patch_bi = |existing: Option<crate::types::ObjectRef>,
                                        signum: i32,
                                        mag_words: &[i32]|
                 -> Option<crate::types::ObjectRef> {
                    let bi = if let Some(e) = existing {
                        e
                    } else {
                        shared.mem.heap.try_alloc_object(class_id, num_fields)?
                    };
                    let mag_arr = shared.mem.heap.alloc_array(
                        ClassId::new(0),
                        ArrayElementType::Int,
                        mag_words.len(),
                    );
                    for (i, w) in mag_words.iter().enumerate() {
                        let _ = shared
                            .mem
                            .heap
                            .set_array_element(mag_arr, i, Value::Int(*w));
                    }
                    shared.mem.heap.set_field(bi, sig_i, Value::Int(signum));
                    shared
                        .mem
                        .heap
                        .set_field(bi, mag_i, Value::Object(Some(mag_arr)));
                    Some(bi)
                };
                let zero = make_or_patch_bi(lookup_existing("ZERO"), 0, &[]);
                let one = make_or_patch_bi(lookup_existing("ONE"), 1, &[1]);
                let two = make_or_patch_bi(lookup_existing("TWO"), 1, &[2]);
                let neg_one = make_or_patch_bi(lookup_existing("NEGATIVE_ONE"), -1, &[1]);
                let ten = make_or_patch_bi(lookup_existing("TEN"), 1, &[10]);
                let mut populated = 0usize;
                if let Some(z) = zero {
                    if set_static_by_name("ZERO", Value::Object(Some(z))) {
                        populated += 1;
                    }
                }
                if let Some(o) = one {
                    if set_static_by_name("ONE", Value::Object(Some(o))) {
                        populated += 1;
                    }
                }
                if let Some(t) = two {
                    if set_static_by_name("TWO", Value::Object(Some(t))) {
                        populated += 1;
                    }
                }
                if let Some(n) = neg_one {
                    if set_static_by_name("NEGATIVE_ONE", Value::Object(Some(n))) {
                        populated += 1;
                    }
                }
                if let Some(t) = ten {
                    if set_static_by_name("TEN", Value::Object(Some(t))) {
                        populated += 1;
                    }
                }
                note_fixup_outcome(
                    "BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN",
                    populated as i64,
                    0,
                    5,
                );
                crate::dispatch_trace::record_note(
                    "Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated",
                );

                // KC16 RBIGDEC.1 follow-up (2026-07-17, hib-defaultcatalog
                // investigation): `smallToString`'s radix-conversion tables
                // (`digitsPerLong`/`longRadix`, populated by JDK's own static
                // array initializers evaluated inside this same `<clinit>`)
                // are casualties of the identical incomplete-clinit failure
                // fixed above for ZERO/ONE/TWO/NEGATIVE_ONE/TEN — but went
                // unnoticed until now because nothing in the suite
                // previously called `BigInteger.toString(radix)` for a
                // radix other than the JDK's own internal fast paths.
                // Hibernate's `NamingHelper.hashedName()` calls
                // `toString(35)` to derive constraint-name hashes, hits
                // `smallToString`'s `longRadix[35]`, and finds a truncated
                // (length-2 instead of 37) array —
                // `ArrayIndexOutOfBoundsException: Index 2 out of bounds for
                // length 2` on `DefaultCatalogAndSchemaTest`'s foreign-key
                // binding path (`CollectionBinder.bindOwnedManyToManyForeignKeyMappedBy`
                // -> `Table.createUniqueKey` -> `ImplicitNamingStrategyJpaCompliantImpl`
                // -> `NamingHelper.generateHashedConstraintName`). Only
                // reproduces when 2+ distinct `@Test` methods run against
                // the same `@ParameterizedClass` instance in one process
                // (confirmed via isolated single-method `MethodRunner`
                // reruns, all 12/12 clean; the failure needs a prior
                // `entityPersister`-style invocation ahead of
                // `createSchema_fromSessionFactory` in the same JVM to
                // surface — consistent with a once-per-process clinit gap
                // rather than a per-call bug). Force-populate both tables
                // with the real JDK's own constants (`java.math.BigInteger`
                // source, radices 2..=36; indices 0/1 are legitimately
                // unused/null per the JDK's own doc comment on these
                // fields).
                const DIGITS_PER_LONG: [i32; 37] = [
                    0, 0, 62, 39, 31, 27, 24, 22, 20, 19, 18, 18, 17, 17, 16, 16, 15, 15, 15, 14,
                    14, 14, 14, 13, 13, 13, 13, 13, 13, 12, 12, 12, 12, 12, 12, 12, 12,
                ];
                const LONG_RADIX_HEX: [u64; 37] = [
                    0,
                    0,
                    0x4000000000000000,
                    0x383d9170b85ff80b,
                    0x4000000000000000,
                    0x6765c793fa10079d,
                    0x41c21cb8e1000000,
                    0x3642798750226111,
                    0x1000000000000000,
                    0x12bf307ae81ffd59,
                    0x0de0b6b3a7640000,
                    0x4d28cb56c33fa539,
                    0x1eca170c00000000,
                    0x780c7372621bd74d,
                    0x1e39a5057d810000,
                    0x5b27ac993df97701,
                    0x1000000000000000,
                    0x27b95e997e21d9f1,
                    0x5da0e1e53c5c8000,
                    0x0b16a458ef403f19,
                    0x16bcc41e90000000,
                    0x2d04b7fdd9c0ef49,
                    0x5658597bcaa24000,
                    0x06feb266931a75b7,
                    0x0c29e98000000000,
                    0x14adf4b7320334b9,
                    0x226ed36478bfa000,
                    0x383d9170b85ff80b,
                    0x5a3c23e39c000000,
                    0x04e900abb53e6b71,
                    0x07600ec618141000,
                    0x0aee5720ee830681,
                    0x1000000000000000,
                    0x172588ad4f5f0981,
                    0x211e44f7d02c1000,
                    0x2ee56725f06e5c71,
                    0x41c21cb8e1000000,
                ];
                let mut radix_fixed = false;
                if let Some(dpl_arr) = shared.mem.heap.try_alloc_array(
                    ClassId::new(0),
                    ArrayElementType::Int,
                    DIGITS_PER_LONG.len(),
                ) {
                    for (i, &d) in DIGITS_PER_LONG.iter().enumerate() {
                        let _ = shared.mem.heap.set_array_element(dpl_arr, i, Value::Int(d));
                    }
                    if set_static_by_name("digitsPerLong", Value::Object(Some(dpl_arr))) {
                        radix_fixed = true;
                    }
                }
                if let Some(lr_arr) = shared.mem.heap.try_alloc_array(
                    class_id,
                    ArrayElementType::Reference,
                    LONG_RADIX_HEX.len(),
                ) {
                    for (i, &hex) in LONG_RADIX_HEX.iter().enumerate() {
                        if i < 2 || hex == 0 {
                            continue;
                        }
                        let mag_words: Vec<i32> = if hex <= 0xFFFF_FFFF {
                            vec![hex as i32]
                        } else {
                            vec![(hex >> 32) as i32, (hex & 0xFFFF_FFFF) as i32]
                        };
                        if let Some(bi) = make_or_patch_bi(None, 1, &mag_words) {
                            let _ = shared.mem.heap.set_array_element(
                                lr_arr,
                                i,
                                Value::Object(Some(bi)),
                            );
                        }
                    }
                    if set_static_by_name("longRadix", Value::Object(Some(lr_arr))) {
                        radix_fixed = true;
                    }
                }
                if radix_fixed {
                    tracing::info!(
                        "Post-clinit fixup: BigInteger digitsPerLong/longRadix radix tables populated"
                    );
                    crate::dispatch_trace::record_note(
                        "Post-clinit fixup: BigInteger digitsPerLong/longRadix radix tables populated",
                    );
                }
            } else {
                tracing::warn!(
                    "Post-clinit fixup: BigInteger fixup skipped — signum/mag field indices not resolved"
                );
                crate::dispatch_trace::record_note("Post-clinit fixup: BigInteger fixup skipped");
            }
        }
        // Spring Boot nested-JAR loaders: `PosixFilePermission.OWNER_READ` etc.
        // must be non-null for `EnumSet.of` / `Set.of` in `JarFileArchive.<clinit>`.
        // When static slots stay null after `<clinit>`, allocate real enum-shaped
        // instances on the loaded `PosixFilePermission` class (jrt-backed).
        "java/util/concurrent/TimeUnit" => {
            const NAMES: &[&str] = &[
                "NANOSECONDS",
                "MICROSECONDS",
                "MILLISECONDS",
                "SECONDS",
                "MINUTES",
                "HOURS",
                "DAYS",
            ];
            let read_static_named = |field_name: &str| -> Option<Value> {
                let cm = shared.classes.class_manager.read();
                let cls = cm.get_class(class_id)?;
                let mut static_idx = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        if &*f.name == field_name {
                            drop(cm);
                            return Some(super::vm_object::get_static_shared(
                                shared, class_id, static_idx,
                            ));
                        }
                        static_idx += 1;
                    }
                }
                None
            };
            let find_instance_field_index = |field_name: &str| -> Option<usize> {
                let cm = shared.classes.class_manager.read();
                let store = &cm.class_store;
                let mut current_id = Some(class_id);
                while let Some(cid) = current_id {
                    if let Some(class) = store.get(cid) {
                        let mut instance_offset = 0usize;
                        for field in &class.fields {
                            if field.is_static() {
                                continue;
                            }
                            if &*field.name == field_name {
                                return Some(class.first_field_index + instance_offset);
                            }
                            instance_offset += 1;
                        }
                        current_id = class.superclass;
                    } else {
                        break;
                    }
                }
                None
            };
            let num_fields = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id)
                    .map(|c| c.num_total_fields)
                    .unwrap_or(0)
            };
            let ordinal_idx = find_instance_field_index("ordinal").unwrap_or(0);
            let name_idx = find_instance_field_index("name");
            let alloc_fields = num_fields
                .max(ordinal_idx + 1)
                .max(name_idx.map_or(0, |i| i + 1));

            let mut filled = 0usize;
            for (ord, &name) in NAMES.iter().enumerate() {
                if matches!(read_static_named(name), Some(Value::Object(Some(_)))) {
                    continue;
                }
                let Some(obj) = shared.mem.heap.try_alloc_object(class_id, alloc_fields) else {
                    continue;
                };
                shared
                    .mem
                    .heap
                    .set_field(obj, ordinal_idx, Value::Int(ord as i32));
                if let Some(name_idx) = name_idx {
                    let nm = super::vm_object::create_java_string(shared, name);
                    shared
                        .mem
                        .heap
                        .set_field(obj, name_idx, Value::Object(Some(nm)));
                }
                if set_static_by_name(name, Value::Object(Some(obj))) {
                    filled += 1;
                }
            }
            if filled > 0 {
                // Not `note_fixup_outcome`: the loop above SKIPS a constant that
                // is already populated, so `filled < NAMES.len()` is the normal
                // healthy answer here and would read as a shortfall.
                tracing::info!(
                    "Post-clinit fixup: TimeUnit backfilled {filled}/{} enum statics",
                    NAMES.len()
                );
            }
            if !matches!(read_static_named("$VALUES"), Some(Value::Object(Some(_)))) {
                if let Some(values_arr) = shared.mem.heap.try_alloc_array(
                    class_id,
                    ArrayElementType::Reference,
                    NAMES.len(),
                ) {
                    for (i, &name) in NAMES.iter().enumerate() {
                        if let Some(Value::Object(Some(o))) = read_static_named(name) {
                            let _ = shared.mem.heap.set_array_element(
                                values_arr,
                                i,
                                Value::Object(Some(o)),
                            );
                        }
                    }
                    if !set_static_by_name("$VALUES", Value::Object(Some(values_arr))) {
                        let _ = set_static_by_name("ENUM$VALUES", Value::Object(Some(values_arr)));
                    }
                }
            }
        }
        "java/nio/file/attribute/PosixFilePermission" => {
            const NAMES: &[&str] = &[
                "OWNER_READ",
                "OWNER_WRITE",
                "OWNER_EXECUTE",
                "GROUP_READ",
                "GROUP_WRITE",
                "GROUP_EXECUTE",
                "OTHERS_READ",
                "OTHERS_WRITE",
                "OTHERS_EXECUTE",
            ];
            let read_static_named = |field_name: &str| -> Option<Value> {
                let cm = shared.classes.class_manager.read();
                let cls = cm.get_class(class_id)?;
                let mut static_idx = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        if &*f.name == field_name {
                            drop(cm);
                            return Some(super::vm_object::get_static_shared(
                                shared, class_id, static_idx,
                            ));
                        }
                        static_idx += 1;
                    }
                }
                None
            };
            if matches!(
                read_static_named("OWNER_READ"),
                Some(Value::Object(Some(_)))
            ) {
                return;
            }
            let ord_idx = {
                let cm = shared.classes.class_manager.read();
                let store = &cm.class_store;
                let mut current_id = Some(class_id);
                let mut found = None;
                while let Some(cid) = current_id {
                    if let Some(class) = store.get(cid) {
                        let mut instance_offset = 0;
                        for field in &class.fields {
                            if field.is_static() {
                                continue;
                            }
                            if &*field.name == "ordinal" {
                                found = Some(class.first_field_index + instance_offset);
                                break;
                            }
                            instance_offset += 1;
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                    current_id = store.get(cid).and_then(|c| c.superclass);
                }
                found
            };
            let name_idx = {
                let cm = shared.classes.class_manager.read();
                let store = &cm.class_store;
                let mut current_id = Some(class_id);
                let mut found = None;
                while let Some(cid) = current_id {
                    if let Some(class) = store.get(cid) {
                        let mut instance_offset = 0;
                        for field in &class.fields {
                            if field.is_static() {
                                continue;
                            }
                            if &*field.name == "name" {
                                found = Some(class.first_field_index + instance_offset);
                                break;
                            }
                            instance_offset += 1;
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                    current_id = store.get(cid).and_then(|c| c.superclass);
                }
                found
            };
            let num_fields = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id)
                    .map(|c| c.num_total_fields)
                    .unwrap_or(0)
            };
            let (Some(ord_idx), Some(name_idx)) = (ord_idx, name_idx) else {
                tracing::warn!(
                    "Post-clinit fixup: PosixFilePermission skipped — ordinal/name field indices not resolved"
                );
                return;
            };
            let mut filled = 0usize;
            for (ord, &name) in NAMES.iter().enumerate() {
                if matches!(read_static_named(name), Some(Value::Object(Some(_)))) {
                    continue;
                }
                let Some(obj) = shared.mem.heap.try_alloc_object(class_id, num_fields) else {
                    continue;
                };
                shared
                    .mem
                    .heap
                    .set_field(obj, ord_idx, Value::Int(ord as i32));
                let nm = super::vm_object::create_java_string(shared, name);
                shared
                    .mem
                    .heap
                    .set_field(obj, name_idx, Value::Object(Some(nm)));
                if set_static_by_name(name, Value::Object(Some(obj))) {
                    filled += 1;
                }
            }
            if filled > 0 {
                // Same reading as the TimeUnit arm: the loop skips constants
                // that are already populated, so a partial count is healthy.
                tracing::info!(
                    "Post-clinit fixup: PosixFilePermission backfilled {filled}/{} enum statics",
                    NAMES.len()
                );
            }
            // `EnumSet` / `Class.getEnumConstants` read the synthetic `$VALUES`
            // array. Without it, `EnumSet.of(OWNER_READ, ...)` throws CCE
            // ("not an enum") even when the named static fields are populated.
            if let Some(values_arr) =
                shared
                    .mem
                    .heap
                    .try_alloc_array(class_id, ArrayElementType::Reference, NAMES.len())
            {
                for (i, &name) in NAMES.iter().enumerate() {
                    if let Some(Value::Object(Some(o))) = read_static_named(name) {
                        let _ = shared.mem.heap.set_array_element(
                            values_arr,
                            i,
                            Value::Object(Some(o)),
                        );
                    }
                }
                if !set_static_by_name("$VALUES", Value::Object(Some(values_arr))) {
                    let _ = set_static_by_name("ENUM$VALUES", Value::Object(Some(values_arr)));
                }
            }
        }
        // KC16 RKC16N.14 — `java/math/BigDecimal.<clinit>` may itself swallow
        // an exception (e.g. propagated from BigInteger or from `SharedSecrets`
        // accessor lookup).  Populate ZERO / ONE / TWO / TEN with minimally-
        // valid BigDecimal instances whose intCompact slots match the JDK so
        // arithmetic on the constants (`BigDecimal.ONE.add(TEN)`) returns
        // sensible values.  Layout per `javap -p java.math.BigDecimal`:
        //   intVal:BigInteger, scale:int, precision:int, stringCache:String,
        //   intCompact:long.
        // Java stamps `INFLATED = Long.MIN_VALUE` as a sentinel meaning "use
        // intVal"; we use the compact path with the literal numeric value so
        // `intValueExact` / `longValue` / `add` work without dereferencing
        // intVal at all in the small-integer fast paths.
        "java/math/BigDecimal" => {
            // Honour the full slot count (inherited + own) so `try_alloc_object`
            // matches the layout that resolve_field_index_in_hierarchy expects.
            let (intval_idx, scale_idx, prec_idx, intcompact_idx, num_fields) = {
                let cm = shared.classes.class_manager.read();
                if let Some(cls) = cm.get_class(class_id) {
                    let mut iv = None;
                    let mut sc = None;
                    let mut pr = None;
                    let mut ic = None;
                    let mut instance_offset = 0usize;
                    for f in cls.fields.iter() {
                        if f.is_static() {
                            continue;
                        }
                        match &*f.name {
                            "intVal" => iv = Some(cls.first_field_index + instance_offset),
                            "scale" => sc = Some(cls.first_field_index + instance_offset),
                            "precision" => pr = Some(cls.first_field_index + instance_offset),
                            "intCompact" => ic = Some(cls.first_field_index + instance_offset),
                            _ => {}
                        }
                        instance_offset += 1;
                    }
                    (iv, sc, pr, ic, cls.num_total_fields)
                } else {
                    (None, None, None, None, 0)
                }
            };
            // Locate BigInteger ZERO/ONE/TWO/TEN if they were already
            // populated (either by a successful BigInteger.<clinit> or by
            // the fixup above).  These become the `intVal` slot for our
            // synthetic BigDecimal constants.
            let bi_class_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_bootstrap_class_by_name("java/math/BigInteger")
            };
            let lookup_bi_static = |name: &str| -> Option<crate::types::ObjectRef> {
                let bi_cid = bi_class_id?;
                let cm = shared.classes.class_manager.read();
                let cls = cm.get_class(bi_cid)?;
                // Static-only indexing — see set_static_by_name comment.
                let mut static_idx = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        if &*f.name == name {
                            drop(cm);
                            match super::vm_object::get_static_shared(shared, bi_cid, static_idx) {
                                Value::Object(Some(o)) => return Some(o),
                                _ => return None,
                            }
                        }
                        static_idx += 1;
                    }
                }
                None
            };
            if let (Some(iv_i), Some(sc_i), Some(pr_i), Some(ic_i)) =
                (intval_idx, scale_idx, prec_idx, intcompact_idx)
            {
                // RBIGDEC.1 — populate the real-JDK layout directly.  No
                // synthetic-stub overlay is needed: `bd_read` in
                // `native-builtins/src/lib.rs` now reads `intCompact` (or
                // falls back to `intVal`) and applies `scale` itself.
                let make_bd = |bi: Option<crate::types::ObjectRef>,
                               compact: i64,
                               scale: i32,
                               prec: i32|
                 -> Option<crate::types::ObjectRef> {
                    let bd = shared.mem.heap.try_alloc_object(class_id, num_fields)?;
                    shared.mem.heap.set_field(bd, iv_i, Value::Object(bi));
                    shared.mem.heap.set_field(bd, sc_i, Value::Int(scale));
                    shared.mem.heap.set_field(bd, pr_i, Value::Int(prec));
                    shared.mem.heap.set_field(bd, ic_i, Value::Long(compact));
                    Some(bd)
                };
                let zero_bd = make_bd(lookup_bi_static("ZERO"), 0, 0, 1);
                let one_bd = make_bd(lookup_bi_static("ONE"), 1, 0, 1);
                let two_bd = make_bd(lookup_bi_static("TWO"), 2, 0, 1);
                let ten_bd = make_bd(lookup_bi_static("TEN"), 10, 0, 2);
                let mut populated = 0usize;
                if let Some(z) = zero_bd {
                    if set_static_by_name("ZERO", Value::Object(Some(z))) {
                        populated += 1;
                    }
                }
                if let Some(o) = one_bd {
                    if set_static_by_name("ONE", Value::Object(Some(o))) {
                        populated += 1;
                    }
                }
                if let Some(t) = two_bd {
                    if set_static_by_name("TWO", Value::Object(Some(t))) {
                        populated += 1;
                    }
                }
                if let Some(t) = ten_bd {
                    if set_static_by_name("TEN", Value::Object(Some(t))) {
                        populated += 1;
                    }
                }
                note_fixup_outcome("BigDecimal ZERO/ONE/TWO/TEN", populated as i64, 0, 4);
            } else {
                tracing::warn!(
                    "Post-clinit fixup: BigDecimal fixup skipped — intVal/scale/precision/intCompact field indices not resolved"
                );
            }
        }
        // R55 (WildFly): `org/jboss/msc/service/ServiceContainerImpl.<clinit>`
        // can throw NPE downstream of a swallowed `ServiceLogger.<clinit>`
        // (ServiceLogger.ROOT is left null after its own clinit swallow,
        // then `invokeinterface ServiceLogger.greeting` on the null ROOT
        // NPEs at SCI<clinit> pc=70). Our swallow then marks SCI as
        // initialized with PARTIAL state: SERIAL is set (pc=24, before the
        // failure point) but `executorSeq` (pc=83, after) and other later
        // statics are NEVER assigned. Even worse, the post-swallow scan
        // here can run BEFORE SCI<clinit>'s pc=24 in some interleaved
        // class-load paths, leaving SERIAL itself null. Downstream
        // `ServiceContainerImpl.<init>` line 143 does `getstatic SERIAL`
        // + `invokevirtual AtomicInteger.getAndIncrement` and NPEs with
        // "Cannot invoke getAndIncrement on null" — the exact failure
        // that aborts WildFly boot.
        //
        // Backfill the two AtomicInteger statics (`SERIAL`, `executorSeq`)
        // with `new AtomicInteger(1)` instances using the 1-field layout
        // registered by `native-builtins/src/phases_early.rs` (field 0 =
        // int value). Only writes when the slot is currently null/zero,
        // so a successful real-clinit pass is never clobbered.
        "org/jboss/msc/service/ServiceContainerImpl" => {
            let ai_name = "java/util/concurrent/atomic/AtomicInteger";
            let ai_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_bootstrap_class_by_name(ai_name)
            };
            let read_static = |field_name: &str| -> Option<Value> {
                let cm = shared.classes.class_manager.read();
                let cls = cm.get_class(class_id)?;
                let mut idx = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        if &*f.name == field_name {
                            return Some(super::vm_object::get_static_shared(
                                shared, class_id, idx,
                            ));
                        }
                        idx += 1;
                    }
                }
                None
            };
            if let Some(aid) = ai_id {
                for fname in ["SERIAL", "executorSeq"] {
                    let cur = read_static(fname);
                    let needs_fix = matches!(cur, Some(Value::Object(None)) | None);
                    if needs_fix {
                        if let Some(ai_obj) = shared.mem.heap.try_alloc_object(aid, 1) {
                            // Initial value 1, matching SCI<clinit> bytecode
                            // (`new AtomicInteger / dup / iconst_1 / <init>(I)V`).
                            // JDK-ONLY-LAYOUT: safe — verified against JDK 25,
                            // `AtomicInteger` declares one instance field
                            // (`private volatile int value`) and its superclass
                            // `java/lang/Number` declares none, so slot 0 is
                            // unambiguous. The hard-coded field COUNT of 1 is
                            // likewise correct; prefer `num_total_fields` if
                            // this is ever generalised to other atomics
                            // (`AtomicReference` = 1, but `AtomicMarkableReference`
                            // and `AtomicStampedReference` are not).
                            shared.mem.heap.set_field(ai_obj, 0, Value::Int(1));
                            if set_static_by_name(fname, Value::Object(Some(ai_obj))) {
                                tracing::info!(
                                    "Post-clinit fixup: ServiceContainerImpl.{} populated with AtomicInteger(1)",
                                    fname
                                );
                            }
                        }
                    }
                }
            }
        }
        // R55 (WildFly): `org/jboss/msc/service/ServiceLogger.<clinit>` calls
        // `Logger.getMessageLogger(Class, String)` which throws
        // IllegalArgumentException in our VM (deep in jboss-logging's
        // dynamic-proxy plumbing). The swallow then leaves the three static
        // ServiceLogger fields (ROOT/SERVICE/FAIL) null. Downstream callers
        // do `getstatic ServiceLogger.ROOT` + `invokeinterface
        // ServiceLogger.greeting(...)` and NPE — the swallow cascade then
        // surfaces deeper as the SCI<init> getAndIncrement NPE (because
        // SCI<clinit> aborts mid-body before pc=24 putstatic SERIAL).
        //
        // Backfill with `ServiceLogger_$logger` instances (the concrete
        // jboss-logging-generated implementation). The `log` instance field
        // (slot 0) is left null — the only ServiceLogger method invoked from
        // SCI<clinit> is `greeting()`, which our VM dispatches via the
        // normal invokeinterface path; the inner `log.logf(...)` is itself
        // protected by the broader <clinit>-swallow if it NPEs again. The
        // critical bit is that ROOT/SERVICE/FAIL are non-null so SCI<clinit>
        // can reach its `putstatic SERIAL` at pc=24.
        // R63 (WildFly): `org/wildfly/security/auth/server/_private/ElytronMessages.<clinit>`
        // calls `Logger.getMessageLogger(Class, String)` which throws
        // IllegalArgumentException in our VM (same jboss-logging dynamic-proxy
        // path that breaks ServiceLogger). The swallow leaves
        // `ElytronMessages.log` null, then `SecurityDomain$Builder.build`
        // does `getstatic ElytronMessages.log` + `invokeinterface
        // isTraceEnabled()` and NPEs at SecurityDomain.java:1100. WildFly
        // catches the NPE and aborts with exit code 1. Backfill the `log`
        // static with a synthetic `ElytronMessages_$logger` instance —
        // mirrors the ServiceLogger fixup just below.
        "org/wildfly/security/auth/server/_private/ElytronMessages" => {
            let impl_name = "org/wildfly/security/auth/server/_private/ElytronMessages_$logger";
            let impl_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_class_by_name_for_class(impl_name, class_id)
            };
            let target_id = impl_id.unwrap_or(class_id);
            // ElytronMessages_$logger extends DelegatingBasicLogger which has
            // one instance field (log:BasicLogger). Allocate with 2 slots to
            // be safe — extra slots are harmless, missing slots NPE on access.
            let num_fields = 2usize;
            let cur = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id).and_then(|cls| {
                    let mut idx = 0usize;
                    for f in &cls.fields {
                        if f.is_static() {
                            if &*f.name == "log" {
                                return Some(super::vm_object::get_static_shared(
                                    shared, class_id, idx,
                                ));
                            }
                            idx += 1;
                        }
                    }
                    None
                })
            };
            let needs_fix = matches!(cur, Some(Value::Object(None)) | None);
            if needs_fix {
                if let Some(obj) = shared.mem.heap.try_alloc_object(target_id, num_fields) {
                    if set_static_by_name("log", Value::Object(Some(obj))) {
                        tracing::info!(
                            "Post-clinit fixup: ElytronMessages.log populated with synthetic $logger"
                        );
                    }
                }
            }
        }
        // (Removed) KC26 `org/keycloak/common/Profile` /
        // `org/keycloak/config/FeatureOptions` arm — see the matching
        // call-site removal in this file's `<clinit>`-success hook.
        // It stamped `Profile.FEATURES` with an empty `HashMap`, which
        // made `Profile.getOrderedFeatures()` skip its real lazy-init
        // and `Profile.isFeatureEnabled` NPE on `null.booleanValue()`.
        // Removed per the no-synthetic-stubs policy; the real `Profile`
        // bytecode now populates `FEATURES`.
        "org/jboss/msc/service/ServiceLogger" => {
            let impl_name = "org/jboss/msc/service/ServiceLogger_$logger";
            let impl_id = {
                let cm = shared.classes.class_manager.read();
                cm.find_class_by_name_for_class(impl_name, class_id)
            };
            // Fall back to allocating on the interface's own class_id when
            // the generated impl isn't loaded (defensive — should be rare).
            let target_id = impl_id.unwrap_or(class_id);
            // ServiceLogger_$logger has 1 instance field (log:Logger).
            let num_fields = 1usize;
            for fname in ["ROOT", "SERVICE", "FAIL"] {
                // Only fix nulls.
                let cur = {
                    let cm = shared.classes.class_manager.read();
                    cm.get_class(class_id).and_then(|cls| {
                        let mut idx = 0usize;
                        for f in &cls.fields {
                            if f.is_static() {
                                if &*f.name == fname {
                                    return Some(super::vm_object::get_static_shared(
                                        shared, class_id, idx,
                                    ));
                                }
                                idx += 1;
                            }
                        }
                        None
                    })
                };
                let needs_fix = matches!(cur, Some(Value::Object(None)) | None);
                if needs_fix {
                    if let Some(obj) = shared.mem.heap.try_alloc_object(target_id, num_fields) {
                        if set_static_by_name(fname, Value::Object(Some(obj))) {
                            tracing::info!(
                                "Post-clinit fixup: ServiceLogger.{} populated with synthetic $logger",
                                fname
                            );
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Per-OS-thread frame-trace publication for the crash handler
// ---------------------------------------------------------------------------

/// The shape `JvmThread::frame_trace` has: the interpreter republishes into it
/// at every blocking / safepoint deposit point, and the `ThreadRegistry` holds
/// the same `Arc` so cross-thread dumps can read it.
pub type PublishedTraceHandle =
    std::sync::Arc<parking_lot::Mutex<Vec<cratonvm_native_api::StackTraceEntry>>>;

std::thread_local! {
    /// The frame trace of whatever Java thread is *currently running on this OS
    /// thread*, for the crash handler to render.
    ///
    /// `crash_handler` already keeps a process-wide
    /// `PRIMORDIAL_FRAME_TRACE` `OnceLock`, published from `Vm::new`. That
    /// covers the common case and nothing else: a fault on a spawned worker, or
    /// on a carrier running a virtual-thread continuation, produced a report
    /// with the *primordial* thread's frames or none at all — and the two crash
    /// classes that most need a Java stack (virtual-thread resume heap
    /// corruption, STW-takeover deadlock) both fault on workers. See
    /// `startup-and-diagnostics.md` §6.3.
    ///
    /// Thread-local rather than a shared registry, deliberately: the crash
    /// handler runs *on the faulting thread* (Rust panic hook, Windows vectored
    /// exception handler), so a TLS read needs no lock at all and cannot be the
    /// thing that turns a diagnosable crash into a hang. The only lock involved
    /// is the trace's own mutex, and that is `try_lock`-only.
    ///
    /// Holds `(thread name, ThreadId, handle)`; the identity is carried because
    /// a carrier OS thread hosts many virtual threads over its life and the
    /// report must say *which* one was mounted.
    static CURRENT_THREAD_FRAME_TRACE: std::cell::RefCell<
        Option<(String, u64, PublishedTraceHandle)>,
    > = std::cell::RefCell::new(None);
}

/// RAII publication of the running Java thread's frame trace into this OS
/// thread's TLS cell.
///
/// Save-and-restore rather than set-and-clear: a virtual-thread carrier mounts
/// one continuation after another (and, in principle, could publish around a
/// nested run), so `Drop` must put back whatever was there before rather than
/// blanking the cell. Every early return out of a mount — the
/// `ContinuationYield` unmount, the uncaught-exception path, the
/// `java_thread_obj`-missing bail — is covered for free, which is the reason
/// this is a guard and not a pair of calls.
#[must_use = "dropping the guard immediately un-publishes the frame trace"]
pub struct PublishedFrameTrace {
    previous: Option<(String, u64, PublishedTraceHandle)>,
}

impl PublishedFrameTrace {
    /// Publish `trace` as this OS thread's Java stack for the duration of the
    /// returned guard. Never panics: if the TLS cell is unavailable (thread
    /// teardown) or already borrowed, publication is silently skipped and the
    /// crash handler falls back to the primordial trace.
    pub fn publish(thread_name: &str, thread_id: u64, trace: PublishedTraceHandle) -> Self {
        let previous = CURRENT_THREAD_FRAME_TRACE
            .try_with(|cell| {
                cell.try_borrow_mut()
                    .ok()
                    .and_then(|mut slot| slot.replace((thread_name.to_string(), thread_id, trace)))
            })
            .ok()
            .flatten();
        Self { previous }
    }
}

impl Drop for PublishedFrameTrace {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = CURRENT_THREAD_FRAME_TRACE.try_with(|cell| {
            if let Ok(mut slot) = cell.try_borrow_mut() {
                *slot = previous;
            }
        });
    }
}

/// The Java frames of the thread currently mounted on *this* OS thread,
/// rendered for a crash report. `None` when nothing is published here — the
/// caller should then fall back to `crash_handler`'s primordial trace.
///
/// Never blocks and never panics: `try_with` + `try_borrow` + `try_lock`
/// throughout. A crash may well have happened while this very thread held the
/// frame-trace mutex, and waiting on it would hang the report.
///
/// Like the primordial renderer this is a *deposit-point* snapshot, not a live
/// walk, and the text says so rather than implying otherwise.
pub fn faulting_thread_java_stack_lines(max_frames: usize) -> Option<Vec<String>> {
    CURRENT_THREAD_FRAME_TRACE
        .try_with(|cell| {
            let slot = cell.try_borrow().ok()?;
            let (name, tid, trace) = slot.as_ref()?;
            let Some(frames) = trace.try_lock() else {
                return Some(vec![format!(
                    "Java frames (faulting thread {name:?}, tid {tid}): \
                     <frame-trace mutex was held at crash time; not waiting on it>"
                )]);
            };
            if frames.is_empty() {
                return Some(vec![format!(
                    "Java frames (faulting thread {name:?}, tid {tid}): <none published yet>"
                )]);
            }
            let mut lines = vec![format!(
                "Java frames (faulting thread {:?}, tid {}, {} frame(s), published at the \
                 last blocking/safepoint deposit — may lag the faulting instruction):",
                name,
                tid,
                frames.len()
            )];
            for entry in frames.iter().take(max_frames) {
                let source = entry.source_file.as_deref().unwrap_or("<unknown>");
                lines.push(format!(
                    "  at {}.{}({}:{})",
                    entry.class_name, entry.method_name, source, entry.line_number
                ));
            }
            if frames.len() > max_frames {
                lines.push(format!("  ... ({} more)", frames.len() - max_frames));
            }
            Some(lines)
        })
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    fn test_shared() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(VmConfig::default()))
    }

    // -----------------------------------------------------------------------
    // default_value_for_descriptor
    // -----------------------------------------------------------------------

    #[test]
    fn default_byte() {
        assert_eq!(default_value_for_descriptor("B"), Value::Int(0));
    }

    #[test]
    fn default_char() {
        assert_eq!(default_value_for_descriptor("C"), Value::Int(0));
    }

    #[test]
    fn default_int() {
        assert_eq!(default_value_for_descriptor("I"), Value::Int(0));
    }

    #[test]
    fn default_short() {
        assert_eq!(default_value_for_descriptor("S"), Value::Int(0));
    }

    #[test]
    fn default_boolean() {
        assert_eq!(default_value_for_descriptor("Z"), Value::Int(0));
    }

    #[test]
    fn default_long() {
        assert_eq!(default_value_for_descriptor("J"), Value::Long(0));
    }

    #[test]
    fn default_float() {
        assert_eq!(default_value_for_descriptor("F"), Value::Float(0.0));
    }

    #[test]
    fn default_double() {
        assert_eq!(default_value_for_descriptor("D"), Value::Double(0.0));
    }

    #[test]
    fn default_object_ref() {
        assert_eq!(
            default_value_for_descriptor("Ljava/lang/String;"),
            Value::Object(None)
        );
    }

    #[test]
    fn default_array_ref() {
        assert_eq!(default_value_for_descriptor("[I"), Value::Object(None));
    }

    #[test]
    fn default_multi_array_ref() {
        assert_eq!(
            default_value_for_descriptor("[[Ljava/lang/Object;"),
            Value::Object(None)
        );
    }

    #[test]
    fn default_empty_descriptor() {
        // Edge case: empty descriptor falls to default match
        assert_eq!(default_value_for_descriptor(""), Value::Int(0));
    }

    #[test]
    fn default_unknown_descriptor() {
        assert_eq!(default_value_for_descriptor("X"), Value::Int(0));
    }

    // -----------------------------------------------------------------------
    // resolve_constant_value
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_integer() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![ConstantPoolEntry::Tombstone, ConstantPoolEntry::Integer(42)];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Int(42)));
    }

    #[test]
    fn resolve_float() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![ConstantPoolEntry::Tombstone, ConstantPoolEntry::Float(1.5)];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Float(1.5)));
    }

    #[test]
    fn resolve_long() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Long(9999999),
            ConstantPoolEntry::Tombstone,
        ];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Long(9999999)));
    }

    #[test]
    fn resolve_double() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Double(3.25),
            ConstantPoolEntry::Tombstone,
        ];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Double(3.25)));
    }

    #[test]
    fn resolve_string_ref_returns_none() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("hello".into()),
            ConstantPoolEntry::StringReference { string_index: 1 },
        ];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 2), None);
    }

    #[test]
    fn resolve_out_of_bounds() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![ConstantPoolEntry::Tombstone];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 99), None);
    }

    #[test]
    fn resolve_negative_integer() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let entries = vec![ConstantPoolEntry::Tombstone, ConstantPoolEntry::Integer(-1)];
        let cp = ConstantPool::new(entries);
        assert_eq!(resolve_constant_value(&cp, 1), Some(Value::Int(-1)));
    }

    // -----------------------------------------------------------------------
    // ensure_class_initialized_shared
    // -----------------------------------------------------------------------

    #[test]
    fn ensure_initialized_nonexistent_class() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        // ClassId that doesn't exist -- state defaults to Loaded, triggers init
        let result = ensure_class_initialized_shared(&shared, &mut thread, ClassId::new(999));
        // This should fail because class 999 doesn't exist
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // ClassStoreHierarchy
    // -----------------------------------------------------------------------

    #[test]
    fn hierarchy_same_class_is_subclass() {
        let shared = test_shared();
        let cm = shared.classes.class_manager.read();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        assert!(hierarchy.is_subclass("java/lang/Object", "java/lang/Object"));
    }

    #[test]
    fn hierarchy_everything_is_subclass_of_object() {
        let shared = test_shared();
        let cm = shared.classes.class_manager.read();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        assert!(hierarchy.is_subclass("java/lang/String", "java/lang/Object"));
        assert!(hierarchy.is_subclass("java/io/PrintStream", "java/lang/Object"));
    }

    #[test]
    fn hierarchy_direct_superclass_uses_linked_class_edge() {
        let shared = test_shared();
        let mut cm = shared.classes.class_manager_write();
        cm.load_class("java/lang/String").unwrap();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        assert!(hierarchy.is_direct_superclass("java/lang/String", "java/lang/Object"));
        assert!(!hierarchy.is_direct_superclass("java/lang/String", "java/io/Serializable"));
    }

    #[test]
    fn hierarchy_common_superclass_same() {
        let shared = test_shared();
        let cm = shared.classes.class_manager.read();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        assert_eq!(
            hierarchy.common_superclass("java/lang/Object", "java/lang/Object"),
            "java/lang/Object"
        );
    }

    #[test]
    fn hierarchy_common_superclass_unknown_returns_object() {
        let shared = test_shared();
        let cm = shared.classes.class_manager.read();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        // For unknown classes, common superclass should be Object
        assert_eq!(
            hierarchy.common_superclass("com/unknown/A", "com/unknown/B"),
            "java/lang/Object"
        );
    }

    #[test]
    fn hierarchy_common_superclass_xstream_exception_siblings() {
        let shared = test_shared();
        let cm = shared.classes.class_manager.read();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        assert_eq!(
            hierarchy.common_superclass(
                "com/thoughtworks/xstream/converters/reflection/ObjectAccessException",
                "com/thoughtworks/xstream/converters/ConversionException",
            ),
            "com/thoughtworks/xstream/converters/ErrorWritingException"
        );
    }

    #[test]
    fn hierarchy_is_interface_unknown() {
        let shared = test_shared();
        let cm = shared.classes.class_manager.read();
        let hierarchy = ClassStoreHierarchy {
            store: &cm.class_store,
        };
        use crate::classloading::vtype::ClassHierarchy;
        // Unknown class should not be considered an interface
        assert!(!hierarchy.is_interface("com/unknown/Foo"));
    }

    // -----------------------------------------------------------------------
    // prepare_class_shared вЂ” ConstantValue initialization (JVMS В§5.5 step 9)
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_class_shared_applies_constant_values_for_all_supported_types() {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::attribute::Attribute;
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use cratonvm_reader::field::ClassFileField;

        // Constant pool layout (1-based):
        //   #1 Integer(42)         -> for IConst
        //   #2 Float(2.5)          -> for FConst
        //   #3 Long(0xDEAD_BEEF_CAFE_BABEu64 as i64)
        //   #4 Tombstone           (Long takes 2 slots)
        //   #5 Double(3.141)
        //   #6 Tombstone           (Double takes 2 slots)
        //   #7 Utf8("hello")
        //   #8 StringReference{string_index: 7}
        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Integer(42),
            ConstantPoolEntry::Float(2.5),
            ConstantPoolEntry::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64),
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Double(3.141),
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8("hello".into()),
            ConstantPoolEntry::StringReference { string_index: 7 },
        ]);

        let make_static_field = |name: &str, descriptor: &str, cv_index: u16| ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC
                | FieldAccessFlags::STATIC
                | FieldAccessFlags::FINAL,
            name: std::sync::Arc::from(name),
            descriptor: std::sync::Arc::from(descriptor),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::ConstantValue {
                    constant_value_index: cv_index,
                },
            )],
        };

        let fields = vec![
            make_static_field("I_CONST", "I", 1),
            make_static_field("F_CONST", "F", 2),
            make_static_field("L_CONST", "J", 3),
            make_static_field("D_CONST", "D", 5),
            make_static_field("S_CONST", "Ljava/lang/String;", 8),
            // Plain static field (no ConstantValue) вЂ” must keep its
            // descriptor-derived default after prepare runs.
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: Arc::from("PLAIN_LONG"),
                descriptor: Arc::from("J"),
                attributes: Vec::new(),
            },
        ];

        let shared = test_shared();
        let class_id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                loader_id: ClassLoaderId::Application,
                name: Arc::from("TestCV"),
                source_file: None,
                version: ClassFileVersion::JAVA_8,
                state: ClassState::Verified,
                initializing_thread: None,
                constant_pool: cp,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                superclass: None,
                interfaces: vec![],
                fields,
                methods: vec![],
                first_field_index: 0,
                num_total_fields: 0,
                bootstrap_methods: vec![],
                signature: None,
                annotations: Vec::new(),
                nest_host: None,
                nest_members: Vec::new(),
                record_components: Vec::new(),
                permitted_subclasses: Vec::new(),
                inner_classes: Vec::new(),
                enclosing_method: None,
                hidden: false,
                module_name: None,
                origin: cratonvm_classloading::ClassOrigin::default(),
                has_finalizer: false,
                code_source: None,
                array_info: None,
                init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
                record_object_methods: std::sync::atomic::AtomicU8::new(0),
            });
            id
        };

        prepare_class_shared(&shared, class_id).expect("prepare_class_shared");

        // Verify each static slot got the right ConstantValue (slots are
        // indexed by position among static fields, i.e. 0..6).
        let statics = shared.classes.statics.read();
        let slots = statics.get(&class_id).expect("statics for class");

        assert_eq!(slots[0], Value::Int(42), "I_CONST");
        assert_eq!(slots[1], Value::Float(2.5), "F_CONST");
        assert_eq!(
            slots[2],
            Value::Long(0xDEAD_BEEF_CAFE_BABEu64 as i64),
            "L_CONST"
        );
        assert_eq!(slots[3], Value::Double(3.141), "D_CONST");

        // S_CONST: must be a non-null String reference.
        match slots[4] {
            Value::Object(Some(obj_ref)) => {
                let text = super::super::vm_object::read_java_string(&shared.mem.heap, obj_ref)
                    .expect("read string");
                assert_eq!(text, "hello", "S_CONST text");
            }
            other => panic!("expected non-null Object for S_CONST, got {other:?}"),
        }

        // PLAIN_LONG: no ConstantValue attribute, must remain Long(0) (zero
        // default for a J descriptor) вЂ” not Object(None) and not Int(0).
        assert_eq!(slots[5], Value::Long(0), "PLAIN_LONG default");
    }

    // -----------------------------------------------------------------------
    // verifier_skip_eligible — HIGH-severity security gate
    //
    // The Pass 2/3 verifier may only be skipped when BOTH (a) the class
    // was loaded by the bootstrap loader AND (b) its name is in a
    // trusted JDK prefix. A user-classpath class that merely *names*
    // itself `java/...` must NOT bypass verification: combined with the
    // interpreter's `_unchecked` operand-stack helpers, an unverified
    // trusted-name class is a sandbox escape.
    // -----------------------------------------------------------------------

    /// Build a minimal `Class` with the given name + loader id for predicate
    /// and end-to-end tests. Uses FINAL+ABSTRACT (a JVMS Г‚В§4.1
    /// "cannot be both" violation) so that if structural verification
    /// runs it will surface a `LinkageError::ClassFormatError`.
    fn make_malformed_class(name: &str, loader_id: cratonvm_types::ClassLoaderId) -> Class {
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::ConstantPool;
        Class {
            id: crate::classloading::ClassId::new(0),
            loader_id,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![]),
            // FINAL + ABSTRACT is rejected by verify_class_access_flags.
            access_flags: ClassAccessFlags::PUBLIC
                | ClassAccessFlags::FINAL
                | ClassAccessFlags::ABSTRACT,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        }
    }

    #[test]
    fn skip_predicate_user_classpath_java_lang_evil_string_is_not_eligible() {
        // User classpath defining a `java/lang/EvilString`: loader is
        // Application, name has a trusted prefix — predicate MUST refuse.
        let c = make_malformed_class(
            "java/lang/EvilString",
            cratonvm_types::ClassLoaderId::Application,
        );
        assert!(
            !verifier_skip_eligible(&c),
            "user-classpath java/lang/EvilString must NOT skip verification"
        );
    }

    #[test]
    fn skip_predicate_bootstrap_java_lang_string_is_eligible() {
        // Real bootstrap-loaded JDK class: skip is the documented
        // perf invariant (HotSpot -Xverify:remote behaviour).
        let c = make_malformed_class("java/lang/String", cratonvm_types::ClassLoaderId::Bootstrap);
        assert!(
            verifier_skip_eligible(&c),
            "bootstrap-loaded java/lang/String must remain eligible for skip"
        );
    }

    #[test]
    fn skip_predicate_bootstrap_application_class_is_not_eligible() {
        // Untrusted prefix even though loaded by bootstrap: skip refuses.
        let c = make_malformed_class("com/example/Foo", cratonvm_types::ClassLoaderId::Bootstrap);
        assert!(!verifier_skip_eligible(&c));
    }

    #[test]
    fn skip_predicate_user_defined_loader_trusted_prefix_is_not_eligible() {
        // Defensive: a UserDefined(_) loader claiming a trusted prefix
        // also must NOT bypass verification.
        let c = make_malformed_class(
            "sun/misc/EvilUnsafe",
            cratonvm_types::ClassLoaderId::UserDefined(7),
        );
        assert!(!verifier_skip_eligible(&c));
    }

    #[test]
    fn end_to_end_user_classpath_trusted_name_triggers_verifier() {
        // End-to-end: install a FINAL+ABSTRACT class named
        // `java/lang/EvilString` with loader=Application into a real
        // ClassStore and run `ensure_class_initialized_shared`.
        // Verification MUST run and surface a Linkage error
        // (`ClassFormatError` from `verify_class_access_flags`).
        use crate::classloading::ClassLoaderId;
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let class_id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            let mut c = make_malformed_class("java/lang/EvilString", ClassLoaderId::Application);
            c.id = id;
            cm.class_store.add(c);
            id
        };
        let result = ensure_class_initialized_shared(&shared, &mut thread, class_id);
        let err = result.expect_err("verifier MUST reject user-classpath java/lang/EvilString");
        match err {
            MethodCallFailed::InternalError(VmError::Linkage(_)) => { /* expected */ }
            other => {
                panic!("expected MethodCallFailed::InternalError(VmError::Linkage), got {other:?}")
            }
        }
    }

    #[test]
    fn clinit_failure_clears_claim_and_removes_waiter() {
        // Regression: an early-return failure inside `initialize_class_shared`
        // (here the bytecode/structural verification failure path) must fully
        // finalize the class via the `InitCleanupGuard` — NOT leak the init
        // claim + waiter. Before the guard, the verify-error path set
        // `state = InitializationError` directly but left `initializing_thread`
        // set and the `class_init_waiters` entry in place, so other threads
        // blocked on the class were never notified and waited out the full
        // timeout.
        use crate::classloading::ClassLoaderId;
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let class_id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            // FINAL+ABSTRACT user-classpath class -> verification runs and fails.
            let mut c = make_malformed_class("com/example/Boom", ClassLoaderId::Application);
            c.id = id;
            cm.class_store.add(c);
            id
        };

        let result = ensure_class_initialized_shared(&shared, &mut thread, class_id);
        assert!(
            result.is_err(),
            "malformed user class must fail initialization; got {result:?}"
        );

        // (1) State is Erroneous, and (2) the init claim is cleared.
        {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(class_id).expect("class still registered");
            assert_eq!(
                class.state,
                ClassState::InitializationError,
                "failed init must leave the class in InitializationError"
            );
            assert_eq!(
                class.initializing_thread, None,
                "failed init must clear the initializing_thread claim (leak guard)"
            );
        }

        // (3) The waiter entry is removed so blocked threads are notified
        // immediately rather than waiting out the timeout. This is the core of
        // the leak the guard fixes.
        assert!(
            !shared
                .classes
                .class_init_waiters
                .lock()
                .contains_key(&class_id),
            "failed init must remove the class_init_waiters entry (waiter leak)"
        );

        // (4) A subsequent init attempt observes the Erroneous state and fails
        // fast (NoClassDefFoundError) instead of re-claiming or blocking.
        let again = ensure_class_initialized_shared(&shared, &mut thread, class_id);
        assert!(
            again.is_err(),
            "re-init of an Erroneous class must fail fast; got {again:?}"
        );
    }

    #[test]
    fn end_to_end_bootstrap_trusted_name_still_skips_verifier() {
        // End-to-end: same malformed shape, but loader=Bootstrap and
        // name in `java/`. The skip is the documented perf invariant вЂ”
        // initialization must succeed (verification is bypassed and
        // structural checks that would have flagged FINAL+ABSTRACT
        // never run).
        use crate::classloading::{ClassLoaderId, ClassState};
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let class_id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            let mut c = make_malformed_class("java/lang/String", ClassLoaderId::Bootstrap);
            c.id = id;
            cm.class_store.add(c);
            id
        };
        let result = ensure_class_initialized_shared(&shared, &mut thread, class_id);
        assert!(
            result.is_ok(),
            "bootstrap-loaded java/lang/String must remain skip-eligible; got {result:?}"
        );
        // Sanity: the class must have reached `Initialized` (no
        // <clinit>, no superclass — full init runs to completion only
        // because verification was skipped).
        let cm = shared.classes.class_manager.read();
        let final_state = cm.get_class(class_id).map(|c| c.state);
        assert_eq!(
            final_state,
            Some(ClassState::Initialized),
            "expected Initialized after skip, got {final_state:?}"
        );
    }

    // -----------------------------------------------------------------------
    // <clinit>-failure policy gate (JVMS §5.5 default-strict inversion)
    //
    // The lenient swallow + post_clinit_fixup path must be OFF by default so
    // a failing `<clinit>` propagates (Erroneous) instead of fabricating
    // synthetic statics. The behavior is opt-in via `CRATONVM_LENIENT_CLINIT=1`.
    // We can only deterministically assert the default here (the gate is a
    // process-lifetime `OnceLock`, so mutating the env mid-test would be
    // racy). When the env var is absent, the gate must read `false`.
    // -----------------------------------------------------------------------

    #[test]
    fn lenient_clinit_defaults_off() {
        // Guard against a polluted CI env explicitly setting the opt-in.
        if cratonvm_types::flags::runtime_var("CRATONVM_LENIENT_CLINIT").as_deref() == Ok("1") {
            // Opt-in is honored — gate reads true. Nothing else to assert.
            assert!(lenient_clinit());
        } else {
            // Default / unset / any non-"1" value => strict (JVMS-correct).
            assert!(
                !lenient_clinit(),
                "lenient <clinit> swallow must be OFF unless CRATONVM_LENIENT_CLINIT=1"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Narrowed lenient-swallow allowlist (`clinit_swallow_has_recovery`).
    //
    // The swallow must be tolerated ONLY for classes with a documented
    // recovery path (a `post_clinit_fixup` arm or the SLF4J/logback native
    // binder stubs) — not for the old broad framework prefix allowlist that
    // hid real init bugs behind a stamped-`Initialized` class.
    // -----------------------------------------------------------------------

    #[test]
    fn clinit_swallow_recovery_admits_documented_cases() {
        // Each of these has an explicit `post_clinit_fixup` match arm.
        for cls in [
            "java/util/logging/LogManager",
            "jdk/internal/icu/text/NormalizerBase$NFCModeImpl",
            "java/lang/invoke/VarHandleInts$Array",
            "java/lang/invoke/VarHandleReferences$Array",
            "io/quarkus/bootstrap/logging/InitialConfigurator",
            "org/jboss/modules/DefaultBootModuleLoaderHolder",
            "java/math/BigInteger",
            "java/math/BigDecimal",
            "java/nio/file/attribute/PosixFilePermission",
            "org/jboss/msc/service/ServiceContainerImpl",
            "org/jboss/msc/service/ServiceLogger",
            "org/wildfly/security/auth/server/_private/ElytronMessages",
        ] {
            assert!(
                clinit_swallow_has_recovery(cls),
                "{cls} has a post_clinit_fixup recovery arm and must stay swallowable",
            );
        }
        // SLF4J/logback binder packages — recovered by native binder stubs.
        assert!(clinit_swallow_has_recovery(
            "org/slf4j/impl/StaticLoggerBinder"
        ));
        assert!(clinit_swallow_has_recovery(
            "ch/qos/logback/classic/util/ContextSelectorStaticBinder"
        ));
    }

    #[test]
    fn clinit_swallow_recovery_rejects_unrecovered_classes() {
        // These matched the OLD broad prefix allowlist (java/ jdk/ sun/ javax/
        // org/jboss/ io/quarkus/ org/wildfly/ com/sun/ org/springframework/boot/loader/)
        // but have NO recovery path. They must now propagate per JVMS §5.5
        // instead of being silently stamped Initialized with null statics.
        for cls in [
            "java/util/HashMap",
            "jdk/internal/misc/Unsafe",
            "sun/security/provider/Sun",
            "javax/crypto/Cipher",
            "org/jboss/modules/Module",
            "io/quarkus/runtime/Application",
            "org/wildfly/common/Assert",
            "com/sun/crypto/provider/SunJCE",
            "org/springframework/boot/loader/jar/JarFileArchive",
            // real-cdi-bean-container Step 3: the Spring startup-metrics no-op
            // shim is removed, so ApplicationStartup runs its real `<clinit>` and
            // must NOT be swallowed/backfilled.
            "org/springframework/core/metrics/ApplicationStartup",
            "org/springframework/core/metrics/DefaultApplicationStartup",
            // Arbitrary app classes were never in the old allowlist and must
            // stay rejected.
            "com/example/MyService",
            "org/apache/catalina/startup/Catalina",
        ] {
            assert!(
                !clinit_swallow_has_recovery(cls),
                "{cls} has no recovery path and must NOT be swallowed",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Per-OS-thread frame-trace publication (startup-and-diagnostics.md §6.3)
    //
    // Every test below runs the publication inside its OWN spawned thread. The
    // cell is thread-local, so doing otherwise would let cargo's test harness
    // (which reuses threads) leak one test's publication into the next.
    // -----------------------------------------------------------------------

    fn entry(class: &str, method: &str, line: i32) -> cratonvm_native_api::StackTraceEntry {
        cratonvm_native_api::StackTraceEntry {
            class_name: class.into(),
            method_name: method.into(),
            source_file: Some("Probe.java".into()),
            line_number: line,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        }
    }

    fn handle(entries: Vec<cratonvm_native_api::StackTraceEntry>) -> PublishedTraceHandle {
        Arc::new(parking_lot::Mutex::new(entries))
    }

    #[test]
    fn an_unpublished_os_thread_reports_nothing_so_the_primordial_path_still_runs() {
        std::thread::spawn(|| {
            assert!(faulting_thread_java_stack_lines(64).is_none());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_published_worker_renders_its_own_frames_with_its_own_identity() {
        std::thread::spawn(|| {
            let _guard = PublishedFrameTrace::publish(
                "pool-1-thread-3",
                77,
                handle(vec![entry("com/example/Svc", "run", 42)]),
            );
            let lines = faulting_thread_java_stack_lines(64).expect("published");
            assert!(
                lines[0].contains("pool-1-thread-3") && lines[0].contains("77"),
                "header must name the faulting thread, got {:?}",
                lines[0]
            );
            assert_eq!(lines[1], "  at com/example/Svc.run(Probe.java:42)");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn dropping_the_guard_unpublishes_so_a_carrier_never_reports_a_stale_mount() {
        std::thread::spawn(|| {
            {
                let _guard =
                    PublishedFrameTrace::publish("vt-1", 1, handle(vec![entry("A", "a", 1)]));
                assert!(faulting_thread_java_stack_lines(8).is_some());
            }
            assert!(
                faulting_thread_java_stack_lines(8).is_none(),
                "an unmounted carrier must not still advertise the continuation's stack"
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_carrier_mounting_one_continuation_after_another_reports_the_current_one() {
        std::thread::spawn(|| {
            for (name, tid) in [("vt-1", 1u64), ("vt-2", 2), ("vt-3", 3)] {
                let _guard = PublishedFrameTrace::publish(
                    name,
                    tid,
                    handle(vec![entry("Task", name, tid as i32)]),
                );
                let lines = faulting_thread_java_stack_lines(8).expect("mounted");
                assert!(lines[0].contains(name), "got {:?}", lines[0]);
                assert!(lines[1].contains(&format!("Task.{name}")));
            }
            assert!(faulting_thread_java_stack_lines(8).is_none());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_nested_publication_restores_the_outer_one_rather_than_blanking_the_cell() {
        std::thread::spawn(|| {
            let _outer =
                PublishedFrameTrace::publish("outer", 10, handle(vec![entry("O", "o", 1)]));
            {
                let _inner =
                    PublishedFrameTrace::publish("inner", 11, handle(vec![entry("I", "i", 2)]));
                assert!(faulting_thread_java_stack_lines(8).unwrap()[0].contains("inner"));
            }
            let lines = faulting_thread_java_stack_lines(8).expect("outer must come back");
            assert!(lines[0].contains("outer"), "got {:?}", lines[0]);
            assert_eq!(lines[1], "  at O.o(Probe.java:1)");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_held_frame_trace_mutex_is_reported_not_waited_on() {
        std::thread::spawn(|| {
            let trace = handle(vec![entry("A", "a", 1)]);
            let _guard = PublishedFrameTrace::publish("worker", 5, trace.clone());
            // Exactly the crash-time situation: the faulting thread was inside
            // a frame-trace republication when it died. Blocking here would
            // turn a diagnosable crash into a hang.
            let _held = trace.lock();
            let lines = faulting_thread_java_stack_lines(8).expect("published");
            assert_eq!(lines.len(), 1);
            assert!(lines[0].contains("not waiting on it"), "got {:?}", lines[0]);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn an_empty_trace_says_so_rather_than_rendering_a_bare_header() {
        std::thread::spawn(|| {
            let _guard = PublishedFrameTrace::publish("worker", 5, handle(Vec::new()));
            let lines = faulting_thread_java_stack_lines(8).expect("published");
            assert_eq!(lines.len(), 1);
            assert!(
                lines[0].contains("<none published yet>"),
                "got {:?}",
                lines[0]
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_deep_stack_is_truncated_with_a_count_of_what_was_dropped() {
        std::thread::spawn(|| {
            let frames: Vec<_> = (0..10).map(|i| entry("C", "m", i)).collect();
            let _guard = PublishedFrameTrace::publish("worker", 5, handle(frames));
            let lines = faulting_thread_java_stack_lines(4).expect("published");
            // header + 4 frames + the "... (6 more)" line
            assert_eq!(lines.len(), 6);
            assert!(lines[5].contains("6 more"), "got {:?}", lines[5]);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn publication_is_per_os_thread_and_never_bleeds_across_workers() {
        let a = std::thread::spawn(|| {
            let _guard = PublishedFrameTrace::publish("A", 1, handle(vec![entry("A", "a", 1)]));
            std::thread::sleep(std::time::Duration::from_millis(20));
            faulting_thread_java_stack_lines(8).expect("A")[0].clone()
        });
        let b = std::thread::spawn(|| {
            let _guard = PublishedFrameTrace::publish("B", 2, handle(vec![entry("B", "b", 2)]));
            std::thread::sleep(std::time::Duration::from_millis(20));
            faulting_thread_java_stack_lines(8).expect("B")[0].clone()
        });
        assert!(a.join().unwrap().contains("\"A\""));
        assert!(b.join().unwrap().contains("\"B\""));
    }
}

#[cfg(test)]
mod post_clinit_fixup_typing_tests {
    use super::coerce_static_to_descriptor;
    use super::typed_default_static_slots;
    use crate::types::Value;

    /// The defect this guards: `Unsafe`'s nine `ARRAY_*_BASE_OFFSET` fields are
    /// declared `J`, the fixup hands them an integer literal, and a slot
    /// holding `Value::Int` where a `long` belongs reads back as garbage from
    /// JIT-compiled code — `Arrays.equals(long[],long[])` answered `true` for
    /// unequal arrays. Assert on the VARIANT, not just the numeric value: an
    /// `assert_eq!(as_i64(...), 16)` passes just as happily on the broken one.
    #[test]
    fn a_long_field_gets_a_long_even_when_the_call_site_writes_an_int() {
        assert!(matches!(
            coerce_static_to_descriptor("J", Value::Int(16)),
            Some(Value::Long(16))
        ));
        assert!(matches!(
            coerce_static_to_descriptor("J", Value::Long(16)),
            Some(Value::Long(16))
        ));
    }

    #[test]
    fn the_narrow_integral_descriptors_stay_int() {
        // `ARRAY_*_INDEX_SCALE` (I), `String.LATIN1`/`UTF16` (B),
        // `UnsafeConstants.BIG_ENDIAN` (Z) all share this arm.
        for d in ["I", "S", "B", "C", "Z"] {
            assert!(
                matches!(
                    coerce_static_to_descriptor(d, Value::Int(4)),
                    Some(Value::Int(4))
                ),
                "descriptor {d}"
            );
            // A long-typed literal narrows rather than being refused: the call
            // sites are integer constants, not user input.
            assert!(
                matches!(
                    coerce_static_to_descriptor(d, Value::Long(4)),
                    Some(Value::Int(4))
                ),
                "descriptor {d}"
            );
        }
    }

    #[test]
    fn floating_descriptors_convert_and_references_pass_through() {
        assert!(matches!(
            coerce_static_to_descriptor("F", Value::Int(2)),
            Some(Value::Float(f)) if f == 2.0
        ));
        assert!(matches!(
            coerce_static_to_descriptor("D", Value::Int(2)),
            Some(Value::Double(d)) if d == 2.0
        ));
        assert!(matches!(
            coerce_static_to_descriptor("Ljava/lang/Object;", Value::Object(None)),
            Some(Value::Object(None))
        ));
    }

    /// The whole point of [`typed_default_static_slots`]: a `J`/`D` slot must
    /// NOT come back as `Value::Int(0)`.
    ///
    /// `StaticsBlock::new` zero-fills with `Int(0)`, which the interpreter
    /// widens and JIT-compiled code reads as garbage — it takes the load width
    /// from the descriptor and pulls 8 bytes over a 4-byte payload. Asserting
    /// the WIDTH (the `Value` variant), not the numeric value, is what makes
    /// this test able to fail: every arm below is zero either way.
    #[test]
    fn wide_static_slots_default_to_their_descriptor_width_not_int_zero() {
        let descriptors = ["J", "I", "D", "F", "Ljava/lang/Object;", "[B", "Z"];
        let slots = typed_default_static_slots(&descriptors, descriptors.len());
        assert!(
            matches!(slots[0], Value::Long(0)),
            "J must be Long, got {:?}",
            slots[0]
        );
        assert!(matches!(slots[1], Value::Int(0)));
        assert!(
            matches!(slots[2], Value::Double(d) if d == 0.0),
            "D must be Double"
        );
        assert!(matches!(slots[3], Value::Float(f) if f == 0.0));
        assert!(matches!(slots[4], Value::Object(None)));
        assert!(matches!(slots[5], Value::Object(None)));
        assert!(matches!(slots[6], Value::Int(0)));
    }

    /// The block is historically sized by the class's TOTAL field count while
    /// slot indices are the STATIC-field enumeration order, so the tail is
    /// slack. It must stay allocated (callers index into it) and must not run
    /// off the end when there are fewer descriptors than slots.
    #[test]
    fn the_slack_tail_past_the_static_descriptors_is_kept_and_zeroed() {
        let slots = typed_default_static_slots(&["J"], 4);
        assert_eq!(slots.len(), 4);
        assert!(matches!(slots[0], Value::Long(0)));
        assert!(matches!(slots[3], Value::Int(0)));
        // Fewer slots than descriptors must truncate, not panic.
        assert_eq!(typed_default_static_slots(&["J", "D", "I"], 1).len(), 1);
        assert!(typed_default_static_slots::<&str>(&[], 0).is_empty());
    }

    /// A mismatch the fixup cannot repair must be REFUSED, not written: a
    /// mistyped slot is worse than the well-typed zero a swallowed `<clinit>`
    /// leaves behind, and the caller turns `None` into a visible shortfall in
    /// the `populated (n/18)` warning.
    #[test]
    fn an_impossible_coercion_is_refused() {
        assert!(coerce_static_to_descriptor("Ljava/lang/String;", Value::Int(16)).is_none());
        assert!(coerce_static_to_descriptor("[I", Value::Long(16)).is_none());
        assert!(coerce_static_to_descriptor("J", Value::Object(None)).is_none());
    }
}
