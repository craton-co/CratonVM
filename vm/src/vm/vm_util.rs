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
//! A failure while LINKING (Pass 2 verification, the verifier's catch-type
//! loads, or a supertype's link failure) is not an initialization failure: the
//! class is parked unlinked ([`LINK_FAILED_STATE`]) and the next use links
//! again and throws the same error, as HotSpot does. Linking runs before the superclass's
//! `<clinit>` (see `InitCleanupGuard` and `initialize_class_shared`).
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
/// Wrap the canonical stdin carrier in a `java/io/BufferedInputStream`, so
/// `System.in` matches every real JVM's identity and capability
/// (`markSupported() == true`). WORKER-4-NOTE-4 (2026-08-22): the CARRIER
/// returned by `ensure_system_stdin_object` must stay unwrapped -- its own
/// callers and `Scanner`'s fd fast path want the object whose slot holds the
/// descriptor -- so only the value installed into the STATIC FIELD is
/// wrapped, via a fresh `BufferedInputStream` built through its real
/// constructor. Measured (that note, all five consumption paths, both
/// modes): byte-identical to HotSpot, including
/// `new Scanner(new BufferedInputStream(System.in))`.
///
/// Both `in_ref` and the freshly allocated wrapper are pinned across the
/// allocating calls in between (class load/init of `BufferedInputStream`,
/// the object allocation, and the constructor itself allocates `buf`) --
/// same discipline as `ensure_system_stdin_object`'s own `in_obj`/`fd_obj`
/// pinning above, and for the same reason: a Rust local held across an
/// allocating call is invisible to `collect_roots` and can go stale under a
/// moving young GC.
///
/// **Not over an image whose `BufferedInputStream` takes an `InternalLock`**
/// (JDK 19-23; JDK 21 is the one that matters). This runs from `System`'s own
/// `<clinit>`, long before `initPhase1` publishes `System.props`, and that
/// constructor initialises `jdk.internal.misc.InternalLock`, whose `<clinit>`
/// calls `System.getProperty`: the NPE poisoned `InternalLock` for the whole
/// run, so every later `BufferedInputStream` / `PrintStream` died with
/// `NoClassDefFoundError` and the default `--jdk-only` boot printed nothing.
/// There `System.in` stays the bare carrier until `initPhase1` installs it.
fn wrap_stdin_buffered(
    shared: &SharedVm,
    thread: &mut JvmThread,
    in_ref: ObjectRef,
) -> Option<ObjectRef> {
    if stdin_wrap_would_init_internal_lock(shared) {
        return None;
    }
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(in_ref);

    let bis_class_id = (|| {
        let bis_class_id = shared
            .load_class_concurrent("java/io/BufferedInputStream")
            .ok()?;
        ensure_class_initialized_shared(shared, thread, bis_class_id).ok()?;
        Some(bis_class_id)
    })();
    let bis_class_id = match bis_class_id {
        Some(id) => id,
        None => {
            thread.native_pin_roots.truncate(pin_base);
            return None;
        }
    };

    let num_fields = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(bis_class_id)
            .map(|c| c.num_total_fields.max(1))
            .unwrap_or(1)
    };
    // zr z4/ab: fallible on ZGC; a refusal leaves `System.in` the bare
    // carrier (this function's existing "could not wrap" answer).
    let bis_obj = match stdin_alloc_object(shared, bis_class_id, num_fields, "System.in buffer") {
        Ok(o) => o,
        Err(_) => {
            thread.native_pin_roots.truncate(pin_base);
            return None;
        }
    };
    let in_ref = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);

    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(bis_obj);
    let result = super::invoke_special_shared_on_class(
        shared,
        thread,
        bis_class_id,
        "java/io/BufferedInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(bis_obj)), Value::Object(Some(in_ref))],
    );
    let bis_obj = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);

    match result {
        Ok(_) => Some(bis_obj),
        Err(_) => None,
    }
}

/// Does the image's `BufferedInputStream` constructor initialise
/// `jdk.internal.misc.InternalLock` (see [`wrap_stdin_buffered`])? Asked of
/// the runtime image itself, so nothing is loaded or fabricated to answer, and
/// an already-initialised `InternalLock` (it read the properties safely) is
/// no obstacle. `CRATONVM_STDIN_WRAP_INTERNAL_LOCK_GUARD=0` restores the
/// unconditional wrap.
fn stdin_wrap_would_init_internal_lock(shared: &SharedVm) -> bool {
    const LOCK: &str = "jdk/internal/misc/InternalLock";
    if !crate::runtime::env_cache::stdin_wrap_internal_lock_guard() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    if cm.runtime_image_defines_class(LOCK) != Some(true) {
        return false;
    }
    !cm.get_loaded_class_id(LOCK)
        .and_then(|id| cm.get_class(id))
        .is_some_and(|c| c.state == ClassState::Initialized)
}

/// zr z4/ab: `System.in`'s carrier objects: on ZGC the fallible entry, whose
/// refusal is a catchable `OutOfMemoryError`; elsewhere the infallible entry.
fn stdin_alloc_object(
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    shared
        .mem
        .heap
        .try_alloc_object_full(class_id, num_fields)
        .ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space ({what})"),
            }))
        })
}

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
    // zr z4/ab: the lazy `System.in` carrier is made on its first read
    // (after start-up), and this function already reports failure: a refusal
    // is the `OutOfMemoryError` HotSpot's `new FileInputStream` would throw,
    // not the infallible arm's abort (ZGC; Generational and G1 unchanged).
    let in_obj = stdin_alloc_object(shared, fis_class_id, num_fields, "System.in")?;
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
    let fd_obj = match stdin_alloc_object(shared, fd_class_id, fd_num_fields, "System.in fd") {
        Ok(o) => o,
        Err(e) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e);
        }
    };
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

/// Replace this OS thread's `<clinit>` nesting depth, answering the previous
/// one (interpreter round i6 wave 130, lane L6). A JDK continuation slice run
/// on its CARRIER's OS thread (`CRATONVM_VTHREAD_SLICE_ON_CARRIER`) starts at
/// `0`, as on a fresh runner: a carrier initializer below `enterSpecial` is
/// not the continuation's frame and must not pin its yields
/// (`jdk_continuation::pinned_reason`). The carrier's depth is put back when
/// the slice ends.
pub(crate) fn replace_clinit_depth(depth: u32) -> u32 {
    CLINIT_NESTING_DEPTH.with(|d| d.replace(depth))
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
    // Fast path: after warm-up the overwhelming majority of calls are for
    // classes already fully initialized. `is_class_initialized_via_manager`
    // answers them from the VM's initialized bitmap (one `Acquire` load and
    // a bit test; `VmConfig::class_init_bit_first`, on by default), and only
    // a clear bit pays the locked read of the class's embedded `init_state`
    // (the per-thread memo answers instead with the switch off, `=0`, or for
    // an id past the bitmap's capacity). Round 5 (atomic fast path), round 9
    // (embedded atomic, not the side table), wave 71 / 72 lane L3 (bitmap
    // first, memo retired for covered ids).
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
    //
    // `VmConfig::class_init_slow_one_read` (`CRATONVM_CLASS_INIT_SLOW_ONE_READ`,
    // interpreter round i4 wave 92, lane L3; default off): the name test rides
    // on the state loop's first read below and the guard runs in the claim
    // arm only -- the one arm that CLAIMS -- once per call. Every other arm
    // answers as it did: Initialized returns, a same-thread Initializing is
    // the recursive request (from inside ConstantDescs' own nest, too), a
    // different-thread Initializing waits for the thread that ran the guard
    // when it claimed. The only difference is an erroneous
    // PrimitiveClassDescImpl, whose NoClassDefFoundError no longer first
    // re-asks ConstantDescs' initialization (that already ran, or failed,
    // when the class was claimed).
    let one_read = shared.config.class_init_slow_one_read;
    if !one_read {
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

    // (`AccessibleObject` is initialised at boot, as HotSpot does, by
    // `Vm::initialize_java_lang_reflect_classes`; no per-class special case.)

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

    // `class_init_slow_one_read`: the ConstantDescs guard is still owed (it
    // runs at most once per call; with the switch off it already ran above).
    let mut cycle_guard_owed = one_read;
    loop {
        let (state, init_thread, guard_now) = {
            let cm = shared.classes.class_manager.read();
            match cm.get_class(class_id) {
                Some(c) => (
                    c.state,
                    c.initializing_thread,
                    cycle_guard_owed
                        && &*c.name == "jdk/internal/constant/PrimitiveClassDescImpl",
                ),
                None => (ClassState::Loaded, None, false),
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
                    // Interpreter round i5 wave 100, lane L1
                    // (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`): the deposit
                    // with the quiet leave's key, so the wake below keeps
                    // this snapshot when nothing it describes changed.
                    ctx.deposit_root_snapshot_keyed();
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
                    let _kept = ctx.check_post_block_gc_quiet();
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
                return Err(
                    crate::runtime::exceptions::raise_could_not_initialize_class_for(
                        shared,
                        thread,
                        class_id,
                        &class_name,
                    ),
                );
            }

            _ => {
                if guard_now {
                    // `class_init_slow_one_read`: the ConstantDescs cycle
                    // guard (see above), before this thread claims
                    // PrimitiveClassDescImpl; then re-read the state, which
                    // the nested `<clinit>` has usually moved to Initialized.
                    cycle_guard_owed = false;
                    if let Ok(cd_id) =
                        shared.load_class_concurrent("java/lang/constant/ConstantDescs")
                    {
                        let _ = ensure_class_initialized_shared(shared, thread, cd_id);
                    }
                    continue;
                }
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
                                        crate::runtime::exceptions::raise_could_not_initialize_class_for(
                                            shared, thread, class_id, &name,
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
                                    register_init_waiter(shared, class_id);
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
                    if r.is_ok() && init_claim_was_lost(shared, class_id) {
                        // The claim initialized a stub that was upgraded in
                        // place meanwhile (`--compatible`): start over, for
                        // the real class's link and `<clinit>` (wave 62,
                        // lane L5).
                        continue;
                    }
                    return r;
                }
                // Another thread claimed it вЂ” loop back to wait
                continue;
            }
        }
    }
}

/// After a claim's initialization returned `Ok`: is `class_id` still not
/// initialized? Only when the claim was lost to a stub upgrade
/// ([`finalize_class_init`]); the caller then initializes the real class.
/// Each `ClassId` is upgraded at most once, so the retry is bounded. Cold
/// (the slow path's end).
#[inline(never)]
fn init_claim_was_lost(shared: &SharedVm, class_id: ClassId) -> bool {
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some_and(|c| c.state != ClassState::Initialized)
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
/// [`ensure_class_initialized_shared`] or [`is_class_initialized_via_manager`]
/// (the VM's initialized bitmap first, this check under a brief
/// `class_manager.read()` on a clear bit).
#[inline]
pub fn is_class_initialized_fast(class: &Class) -> bool {
    class.init_state.load(std::sync::atomic::Ordering::Acquire)
        == cratonvm_classloading::CLASS_INIT_INITIALIZED
}

thread_local! {
    /// Classes this host thread has already observed fully initialized, scoped
    /// to a `SharedVm` so sequential in-process VMs cannot alias `ClassId`s.
    ///
    /// # Since wave 72: the fallback of the bitmap, not the fast path
    ///
    /// With `VmConfig::class_init_bit_first` on (the default since wave 71)
    /// `is_class_initialized_via_manager` asks this memo only for ids the
    /// VM's `ClassInitBits` does not cover (1 Mi per VM, ids never reused, so
    /// only a process defining over a million classes reaches it). With the
    /// switch off (`CRATONVM_CLASS_INIT_BIT_FIRST=0`) it serves every id, as
    /// documented below. Interpreter round i3 wave 72, lane L3
    /// (`i71-L3-proposal-retire-the-per-thread-class-init-memo-20261104.md`).
    ///
    /// Class initialization is monotonic: once a class reaches
    /// `CLASS_INIT_INITIALIZED` it stays there for the life of the VM (a failed
    /// `<clinit>` poisons the class into an *erroneous* state instead, which
    /// never reads as initialized) -- with ONE exception, a synthetic stub
    /// upgraded in place to its real class file (`--compatible`), whose
    /// `init_state` the upgrade resets so the real `<clinit>` runs. Each entry
    /// therefore carries the VM's init-reset generation
    /// (`ClassInitBits::reset_generation`) loaded BEFORE the read that found
    /// the class initialized, and answers only while the generation still
    /// reads that value: one load and a compare on a hit (interpreter round i2
    /// wave 61, lane L5; probe `L5W61StubUpgradeInit`). Until then a thread
    /// that had touched the stub skipped the real class's `<clinit>`. Negative
    /// answers are NOT memoized -- they change as soon as `<clinit>` completes.
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
    /// already bounded buys nothing and reintroduces the thrash. A stale
    /// entry (older generation) is overwritten by the next positive read.
    static CLASS_INITIALIZED_MEMO: std::cell::RefCell<rustc_hash::FxHashMap<(usize, u32), u64>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Whether `class_id` is fully initialized, without initializing it.
/// Returns `false` for unknown class ids (the slow path will then surface
/// the appropriate error).
///
/// With `VmConfig::class_init_bit_first` (on by default since wave 71) the
/// answer is the VM's initialized bitmap -- one `Acquire` load and a bit
/// test -- and, on a clear bit, a locked read of the class's embedded
/// `init_state` (`class_manager.read()`) that fills the bit when it finds the
/// class initialized. The per-thread `CLASS_INITIALIZED_MEMO` is consulted
/// only for ids the bitmap does not cover (past 1 Mi per VM), or for every id
/// with the switch off (`=0`, the pre-wave-71 shape).
///
/// The building block of [`ensure_class_initialized_shared`]'s fast path.
/// Exposed separately so callers that want a non-erroring "is C
/// initialized?" probe (reflective / method-handle doors, the JIT's
/// static-field slow paths) can use it without the Err-returning machinery.
#[inline]
pub fn is_class_initialized_via_manager(shared: &SharedVm, class_id: ClassId) -> bool {
    let raw = class_id.as_u32();
    let bits = &shared.classes.class_init_bits;
    // Interpreter round i3 wave 71, lane L3 (`VmConfig::class_init_bit_first`,
    // proposal `i59-L3-proposal-one-branch-class-initialized-test`): the VM's
    // initialized bitmap answers first -- no TLS, no hash. A set bit is valid
    // for every reader: it is set only after the class's `init_state` reached
    // INITIALIZED, and the one reset (a stub upgraded in place) clears it
    // under the class-manager write lock (`ClassInitBits::note_init_reset`).
    let bit_first = shared.config.class_init_bit_first;
    if bit_first && bits.is_set(raw) {
        return true;
    }
    // Wave 72, lane L3 (`i71-L3-proposal-retire-the-per-thread-class-init-memo`):
    // with the bitmap on, a covered id never needs the memo -- every positive
    // answer the memo could hold also set the bit (the fill below, or
    // `finalize_class_init`), and a reset clears both together (the bit, and
    // the memo through the generation). So a clear bit goes straight to the
    // locked read. Same answers; one cache instead of two.
    let memo_serves = !bit_first || !bits.covers(raw);
    // Loaded before the memo probe AND before the locked read below, so an
    // entry (or bit) filled from that read is stamped with a generation no
    // later than the state it saw.
    let generation = bits.reset_generation();
    if memo_serves && class_init_memo_hit(shared.vm_identity, raw, generation) {
        return true;
    }
    let initialized = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(class_id) {
            Some(class) => is_class_initialized_fast(class),
            // A lambda proxy lives in `ClassRealm::lambda_proxies`, not the
            // class manager, and has no `<clinit>`: HotSpot's spun class is
            // initialized before its first instance exists. Answering `false`
            // sent `Method.invoke` on a method looked up on the lambda's own
            // class (`lam.getClass().getMethod(..)`) into
            // `prepare_class_shared`, which refused the id with an internal
            // error. Off the fast path: the bitmap does not cover proxy ids.
            None => shared.classes.is_lambda_proxy_class(class_id),
        }
    };
    if initialized {
        if bit_first {
            // A class marked INITIALIZED outside `finalize_class_init` (boot
            // pre-initialization, a stub's synthetic init) gets its bit from
            // the first locked read that sees it: `set_as_of` clears it again
            // if a reset happened since `generation` was loaded. Cold: once
            // the bit is set no thread misses on this class again.
            bits.set_as_of(raw, generation);
        }
        if memo_serves {
            class_init_memo_fill(shared.vm_identity, raw, generation);
        }
    }
    initialized
}

/// `CLASS_INITIALIZED_MEMO` holds `(vm, raw)` at `generation`. Off the
/// default path (the bitmap answers covered ids).
#[inline(never)]
fn class_init_memo_hit(vm_key: usize, raw: u32, generation: u64) -> bool {
    CLASS_INITIALIZED_MEMO.with(|memo| memo.borrow().get(&(vm_key, raw)) == Some(&generation))
}

/// Record `(vm, raw)` initialized at `generation` in
/// `CLASS_INITIALIZED_MEMO`.
#[inline(never)]
fn class_init_memo_fill(vm_key: usize, raw: u32, generation: u64) {
    CLASS_INITIALIZED_MEMO.with(|memo| {
        memo.borrow_mut().insert((vm_key, raw), generation);
    });
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
///
/// HotSpot's `has_nonstatic_concrete_methods` (declared or inherited); the
/// "declares" half is [`interface_declares_default_method`].
fn interface_has_default_method(cm: &crate::classloading::ClassManager, iface_id: ClassId) -> bool {
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

/// Does `iface_id` ITSELF declare a non-abstract, non-static method?
/// HotSpot's `declares_nonstatic_concrete_methods`: only such an interface is
/// initialized by a class's initialization (JVMS §5.5 step 7).
fn interface_declares_default_method(
    cm: &crate::classloading::ClassManager,
    iface_id: ClassId,
) -> bool {
    cm.get_class(iface_id)
        .is_some_and(|c| c.methods.iter().any(|m| !m.is_abstract() && !m.is_static()))
}

/// JVMS §5.5 step 7's `SI1, ..., SIn` for the CLASS `class_id`, in the order
/// the step gives: for each interface `I` directly implemented (in `interfaces`
/// order), the enumeration recurs on `I`'s superinterfaces before returning
/// `I`, and only interfaces that DECLARE a default method are listed. The
/// recursion enters only an interface that declares or inherits one — exactly
/// HotSpot's `InstanceKlass::initialize_super_interfaces`. Each interface is
/// listed once. Empty for an interface: step 7 initializes superinterfaces for
/// a class only.
///
/// Interpreter round i1 wave 9 (lane L5): this used to initialize every DIRECT
/// superinterface whose closure declared a default method, including one that
/// only inherited it (so its `<clinit>` ran although HotSpot's never does), and
/// it ran for interfaces too, so initializing an interface initialized its
/// superinterfaces.
fn superinterfaces_to_initialize(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> Vec<ClassId> {
    fn visit(
        cm: &crate::classloading::ClassManager,
        id: ClassId,
        seen: &mut rustc_hash::FxHashSet<ClassId>,
        out: &mut Vec<ClassId>,
        depth: usize,
    ) {
        // A hierarchy is shallow; this bounds a malformed cyclic one.
        if depth > 256 {
            return;
        }
        let Some(c) = cm.get_class(id) else {
            return;
        };
        for &iface in &c.interfaces {
            if interface_has_default_method(cm, iface) {
                visit(cm, iface, seen, out, depth + 1);
            }
            if interface_declares_default_method(cm, iface) && seen.insert(iface) {
                out.push(iface);
            }
        }
    }
    let mut out = Vec::new();
    if cm.get_class(class_id).is_some_and(|c| c.is_interface()) {
        return out;
    }
    let mut seen = rustc_hash::FxHashSet::default();
    visit(cm, class_id, &mut seen, &mut out, 0);
    out
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

/// SBR5 lane jfr: a class finished initializing. When it is a concrete
/// `jdk.jfr.Event` subclass, let the JFR backend register it (HotSpot does the
/// same at that point). The test is a walk of the loaded superclass chain; any
/// other class pays only that.
fn jfr_event_class_initialized(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    init_ns: u64,
) {
    // gci i18/na: neither context below is under `safe_native_call_impl`; a
    // refused allocation drops the JFR event / the event-class registration
    // (Java cannot observe either) instead of the infallible arm's abort.
    if cratonvm_native_builtins::jfr_vm_events::any_recording() {
        let recorded = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
            cratonvm_native_builtins::jfr_vm_events::class_loaded(&mut ctx, class_id, init_ns as i64);
        });
        if let Err(oom) = recorded {
            tracing::debug!(
                "jfr class-loaded event dropped: java.lang.OutOfMemoryError: {}",
                oom.java_message()
            );
        }
    }
    let is_event = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return;
        };
        if class.access_flags.intersects(
            cratonvm_reader::class_access_flags::ClassAccessFlags::INTERFACE
                | cratonvm_reader::class_access_flags::ClassAccessFlags::ABSTRACT,
        ) {
            return;
        }
        let mut current = class.superclass;
        let mut found = false;
        let mut depth = 0;
        while let Some(id) = current {
            let Some(c) = cm.get_class(id) else { break };
            if &*c.name == "jdk/jfr/Event" {
                found = true;
                break;
            }
            current = c.superclass;
            depth += 1;
            if depth > 64 {
                break;
            }
        }
        found
    };
    if !is_event {
        return;
    }
    let registered = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
        let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
        crate::native::builtins::jfr_backend::event_class_initialized(&mut ctx, class_id);
    });
    if let Err(oom) = registered {
        tracing::debug!(
            "jfr event-class registration dropped: java.lang.OutOfMemoryError: {}",
            oom.java_message()
        );
    }
}
fn finalize_class_init(
    shared: &SharedVm,
    class_id: ClassId,
    new_state: ClassState,
    claimant: u64,
) -> bool {
    // (Not for a claim lost to a stub upgrade, whose state is not recorded;
    // `note_lost_init_claim` names it.)
    if matches!(new_state, ClassState::InitializationError)
        && dbg_clinit_fail()
        && shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_none_or(|c| init_claim_held(c, claimant))
    {
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
    // The waiter entry leaves the map under the SAME class-manager write lock
    // that clears the claim, because a claim inserts its entry under that lock
    // too. Removed after the lock was released, a new claimant that came in
    // between (a class parked unlinked is claimable again at once) had its own
    // fresh entry removed and notified here, while this claim's entry was
    // overwritten in the map and never notified: its waiters slept out the
    // 30 s timeout, and the new claimant's waiters found no entry and spun.
    // The init-reset generation as of this write lock: a stub upgrade after
    // the lock drops (it takes the same lock) retires the bit
    // `note_class_initialized` sets below (`ClassInitBits::set_as_of`).
    //
    // A claim can be LOST before it is finalized: a synthetic stub upgraded in
    // place to its real class file while this claim was linking or
    // initializing it (`--compatible`; `ClassManager::upgrade_synthetic_class`
    // resets `state` to `Loaded` and clears `initializing_thread`). Whatever
    // the claim did was done to the stub, so it must not be published for the
    // real class: the state is left as the upgrade (or a later claim) set it,
    // and the waiters are released only when no other claim holds the class
    // (a later claim shares this claim's waiter entry, see
    // `register_init_waiter`, and releases them itself). Interpreter round i2
    // wave 62, lane L5:
    // `interpreter-L5-a-stub-upgrade-during-the-stubs-own-initialization-is-lost-FIXED-20261026.md`.
    let (removed, init_generation, held) = {
        let mut cm = shared.classes.class_manager_write();
        let init_generation = shared.classes.class_init_bits.reset_generation();
        let (held, release_waiter) = match cm.get_class_mut(class_id) {
            Some(class) if class.initializing_thread == Some(claimant) => {
                class.state = new_state;
                class.initializing_thread = None;
                (true, true)
            }
            Some(class) => (false, class.initializing_thread.is_none()),
            None => (true, true),
        };
        if held && matches!(new_state, ClassState::Initialized) {
            cm.set_class_init_state(class_id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
        }
        // A link or initialization completion can settle a held deferred-`new`
        // site: moved while `cm` is still held, so this guard's own release
        // sweep sees it (`CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP`; interpreter
        // round i4 wave 92, lane L7).
        if held {
            crate::runtime::interpreter::jit_bridge::note_new_site_settle_event(shared);
        }
        // A STATEMENT, and `cm` dropped explicitly after it (round 13 wave 10,
        // lane spin, `r13w10-orch-codecache-discard-churn-compatible-spin-hang`).
        // This used to be the block's tail expression, and in edition 2021 a
        // tail expression's temporaries outlive the block's locals: the
        // `class_init_waiters` guard was still held when `cm` dropped, and
        // `ClassManagerWriteGuard::drop` re-takes `class_manager.read()` after
        // releasing the write lock (the deferred-`new` resweep and the profile
        // replay seed). A claimant in `ensure_class_initialized_shared` takes
        // the two locks in the other order (the write lock, then
        // `class_init_waiters` to register its waiter), so the two deadlocked
        // and every other thread parked on its next `class_manager.read()`.
        let removed = if release_waiter {
            shared.classes.class_init_waiters.lock().remove(&class_id)
        } else {
            None
        };
        drop(cm);
        (removed, init_generation, held)
    };
    // JIT round 21 wave 1 (lane indy, INDY21-2): the claim ended (whatever its
    // outcome): no compiled class-init barrier may read inline for it now.
    // After the write lock that cleared `initializing_thread`.
    crate::runtime::interpreter::jit_bridge::clinit_owner_note(shared, class_id, None);
    if !held {
        note_lost_init_claim(shared, class_id);
    }
    if held && matches!(new_state, ClassState::Initialized) {
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
        //
        // JIT round 21 wave 1 (lane indy, INDY21-3): numbered in the static-init
        // log FIRST, so a compile door that sees the bit below set also finds
        // the entry (`jit_bridge::note_static_init_waiters`).
        crate::runtime::interpreter::jit_bridge::static_init_log_class(shared, class_id);
        crate::jit::helpers::note_class_initialized(shared, class_id, init_generation);
        // This class's `static final VarHandle`s are now constants the
        // optimizing tier may fold. After the init memo above, so a compile that
        // sees a plan also sees an initialised class.
        crate::jit::helpers::note_class_initialized_varhandles(shared, class_id);
        // JIT round 20 wave 11 (lane pIR): and the lengths of its trusted
        // `static final` arrays, for the `arraylength` fold.
        crate::jit::helpers::note_class_initialized_final_array_lengths(shared, class_id);
        // And the classes of its trusted `static final` references, for the
        // exact receiver proof (`CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`).
        crate::jit::helpers::note_class_initialized_final_ref_classes(shared, class_id);
        // JIT round 20 wave 17 (lane pStr3, `CRATONVM_JIT_STATIC_INIT_RECOMPILE`):
        // bodies compiled while this class was uninitialized read its statics
        // through the helper; recompile them now that a direct load is legal.
        crate::runtime::interpreter::jit_bridge::static_init_class_initialized(shared, class_id);
    }
    // Notify every thread blocked on this claim.
    if let Some(pair) = removed {
        let (lock, cvar) = &*pair;
        // Round-9 HIGH-4: parking_lot — no poison/unwrap.
        let mut done = lock.lock();
        *done = true;
        cvar.notify_all();
    }
    held
}

/// A claim of `class_id` was lost to a stub upgrade ([`finalize_class_init`]).
/// Cold: at most once per upgraded stub. `CRATONVM_DBG_CLINIT_FAIL` names it
/// (the positive control of the lost-claim fix).
#[cold]
#[inline(never)]
fn note_lost_init_claim(shared: &SharedVm, class_id: ClassId) {
    if dbg_clinit_fail() {
        let name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<class {class_id}>"));
        eprintln!("[DBG_CLINIT_FAIL] {name}: init claim lost to a stub upgrade; retried");
    }
}

/// Register the waiter entry of a new claim of `class_id`, under the
/// class-manager write lock that records the claim. An entry still in the map
/// belongs to a claim that was lost to a stub upgrade and could not release
/// its waiters (another claim held the class by then, see
/// [`finalize_class_init`]): the new claim takes it over, so the threads
/// parked on it are woken when this claim finalizes. Every entry in the map is
/// unfinished (`done` is set only after the entry left the map).
fn register_init_waiter(shared: &SharedVm, class_id: ClassId) {
    shared
        .classes
        .class_init_waiters
        .lock()
        .entry(class_id)
        .or_insert_with(|| {
            std::sync::Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()))
        });
}

/// Does the claim `claimant` still hold `class`? Only a stub upgrade
/// (`ClassManager::upgrade_synthetic_class`) takes a claim away.
#[inline]
fn init_claim_held(class: &Class, claimant: u64) -> bool {
    class.initializing_thread == Some(claimant)
}

/// RAII guard ensuring that a class claimed for initialization is always
/// finalized, even on the early `?`/`return Err(..)` paths inside
/// [`initialize_class_shared`] (verification, superclass init, preparation,
/// superinterface init, `System` stdin setup). If the guarded body returns
/// without reaching a terminal `finalize`, the guard runs
/// `finalize_class_init(.., failure_state)` on drop — clearing the
/// `initializing_thread` claim, removing the waiter, and notifying all waiters.
///
/// `failure_state` is [`LINK_FAILED_STATE`] while the class is being LINKED
/// and `InitializationError` from then on. HotSpot keeps the two apart
/// (`InstanceKlass::initialize_impl` links before it takes the init lock): a
/// link failure (verification, the verifier's catch-type loads) leaves the class
/// unlinked, so the next use links again and throws the same error; only a
/// failure of the initialization proper makes the class erroneous, after which
/// every use answers "Could not initialize class". Waiters woken from a link
/// failure see an unclaimed, not-yet-linked class, claim it and relink on their
/// own thread, as HotSpot's do.
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
    /// The state an early return parks the class in (see the type doc).
    failure_state: &'a std::cell::Cell<ClassState>,
    /// The `initializing_thread` this claim recorded.
    claimant: u64,
}

impl Drop for InitCleanupGuard<'_> {
    fn drop(&mut self) {
        if !self.finalized.get() {
            // The body returned early without finalizing (a propagated `Err`
            // from verification, superclass/interface init, preparation, or the
            // System-streams setup, a claim lost to a stub upgrade, or a panic
            // unwinding through the body). Parked unlinked for a link failure,
            // Erroneous otherwise; either way release every waiter promptly.
            finalize_class_init(
                self.shared,
                self.class_id,
                self.failure_state.get(),
                self.claimant,
            );
        }
    }
}

/// The state a class whose LINKING failed is parked in: `Verifying` with no
/// `initializing_thread` (see [`InitCleanupGuard`]).
///
/// Not `Loaded`, because "never linked" and "failed to link" must stay apart
/// for the readers that skip initialization when it would be unobservable
/// (`jit_bridge::jit_new_target_init_pending`,
/// `exceptions::initialisation_is_unobservable`): a never-linked class with no
/// `<clinit>` and nothing to check at link time may be allocated without
/// initializing it ([`class_linked_or_link_trivial`]), a class known to fail
/// linking may not. Every other reader treats it like `Loaded`: the init path
/// claims it (`ensure_class_initialized_shared`'s catch-all arm) and Step 1
/// re-runs verification, which is gated on `Verifying`.
pub const LINK_FAILED_STATE: ClassState = ClassState::Verifying;

/// Did an earlier attempt to link `class` fail (and no thread is linking it
/// now)? See [`LINK_FAILED_STATE`].
#[inline]
pub fn class_link_failed(class: &Class) -> bool {
    class.state == LINK_FAILED_STATE && class.initializing_thread.is_none()
}

/// Would linking `class` run no check at all, so that skipping its link step
/// is unobservable? The same three exemptions the link step of
/// [`initialize_class_shared`] applies: `-Xverify:none`, a class defined with
/// `skip_verification` (trusted runtime-generated bytecode), and a
/// bootstrap-trusted JDK class outside `-Xverify:all`.
pub fn class_link_is_trivial(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &Class,
) -> bool {
    // An application class-path class may owe the JDK's package step
    // (`define_class_path_package_at_link`), which runs Java and can refuse
    // the class: never trivial while that switch is on (read from `class`,
    // no second class-manager lock).
    if shared.classes.class_path_package_at_link
        && class.loader_id == cratonvm_types::ClassLoaderId::Application
        && class.name.contains('/')
        && shared.natives.native_methods.jdk_built_packages()
    {
        return false;
    }
    // A user-loader class still owes its link the structural scan under
    // `--noverify` (interpreter round i5 wave 103, lane L7).
    (shared.config.skip_verification && !cm.user_pass3_pending(class.id))
        || cm.class_skip_bytecode_verification(class.id)
        || (!cm.strict_verification() && verifier_skip_eligible(class))
}

/// Is `class` linked, or is linking it trivial ([`class_link_is_trivial`]) and
/// not known to fail? What a reader that skips initialization because it
/// would be unobservable must also ask: initialization LINKS the class first,
/// and a link error it owes is observable (`jit_bridge::jit_new_target_init_pending`).
/// `Loading` answers no, and so does a non-trivial class being linked right
/// now. Supertypes are the caller's to walk.
///
/// While `ClassPrepare` is observed ([`class_prepare_observed`]) a never-linked
/// class answers no whatever its checks (wave 15): skipping its initialization
/// would also skip its preparation and the event a debugger or an agent waits
/// for, so the reader initializes it on a Java thread instead.
pub fn class_linked_or_link_trivial(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &Class,
) -> bool {
    match class.state {
        ClassState::Loading => false,
        ClassState::Loaded | ClassState::Verifying => {
            // A trivially linked class still fails to link when a supertype
            // does (`supertype_failed_to_link`).
            !class_link_failed(class)
                && class_link_is_trivial(shared, cm, class)
                && !class_prepare_observed(shared)
        }
        _ => true,
    }
}

/// Does a JDWP `ClassPrepare` request or a C JVMTI agent's `ClassPrepare`
/// enable listen in this VM (interpreter round i1 wave 15)? Then preparing a
/// class is observable, and linking one outside initialization
/// ([`link_class_without_java`], which never prepares) or allocating one
/// without initializing it would hide the class from the listener: the class
/// is left to be initialized — linked, prepared and reported
/// (`initialize_class_shared`) — on a Java thread. Two loads; always `false`
/// without the `experimental-debug` feature.
#[inline]
pub fn class_prepare_observed(shared: &SharedVm) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        let gates = &shared.debug.debugger_gates;
        gates.class_prepare_armed() || gates.native_class_prepare_armed()
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = shared;
        false
    }
}

/// The `initializing_thread` a [`link_class_without_java`] claim records. No
/// `JvmThread` has this id (ids count up from 1, and `u64::MAX` is the "no
/// thread" sentinel elsewhere), so a Java thread that asks to initialize the
/// class meanwhile waits for the claim like for any other thread's.
const LINK_WITHOUT_JAVA_CLAIM: u64 = u64::MAX - 1;

/// Link `class_id` now, outside initialization, when that runs no Java: the
/// link step of [`initialize_class_shared`] (Pass 2 structural verification
/// and, under `--jdk-only`, the catch-type loads) for a class whose catch types
/// are all loaded already. Returns whether the class is linked afterwards.
///
/// For a compile door that must not bake an allocation of a class that was
/// never linked (`jit_bridge::jit_new_target_init_pending`): JVMS §5.4 lets
/// linking happen early as long as its errors surface at the use. So this only
/// ever moves a class FORWARD, to `Verified`. A check that fails, a catch type
/// that would have to be loaded, a class already claimed by a thread, or a
/// panic all leave the class exactly as it was, for the next use to link on a
/// Java thread and raise the error there.
///
/// It claims the class the way initialization does (`initializing_thread`
/// plus a `class_init_waiters` entry, under the class-manager write lock), so
/// it never races an initializing thread; it never waits for one either.
///
/// It does not prepare, so it refuses while `ClassPrepare` is observed
/// ([`class_prepare_observed`], wave 15): the class is then linked, prepared
/// and reported at its first use on a Java thread.
pub fn link_class_without_java(shared: &SharedVm, class_id: ClassId) -> bool {
    let prior_state = {
        let mut cm = shared.classes.class_manager_write();
        let Some(class) = cm.get_class_mut(class_id) else {
            return false;
        };
        if class.initializing_thread.is_some() {
            return false;
        }
        match class.state {
            ClassState::Loaded | ClassState::Verifying => {}
            ClassState::Loading => return false,
            _ => return true,
        }
        if class_prepare_observed(shared) {
            return false;
        }
        let prior_state = class.state;
        class.state = ClassState::Verifying;
        class.initializing_thread = Some(LINK_WITHOUT_JAVA_CLAIM);
        register_init_waiter(shared, class_id);
        prior_state
    };
    let linked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        link_checks_pass_without_java(shared, class_id)
    }))
    .unwrap_or(false);
    // A claim lost to a stub upgrade checked the stub: not linked.
    finalize_class_init(
        shared,
        class_id,
        if linked {
            ClassState::Verified
        } else {
            prior_state
        },
        LINK_WITHOUT_JAVA_CLAIM,
    ) && linked
}

/// The `(package, code source)` of `class_id` when linking it owes the JDK's
/// `defineOrCheckPackage` call ([`define_class_path_package_at_link`]): an
/// application-loader class of a named package with a code source, whose pair
/// the JDK has not accepted yet, with `CRATONVM_CLASS_PATH_PACKAGE_AT_LINK` on.
/// One `bool` load when the switch is off.
#[inline]
fn class_path_package_check_due(shared: &SharedVm, class_id: ClassId) -> Option<(String, String)> {
    if !shared.classes.class_path_package_at_link {
        return None;
    }
    class_path_package_check_key(shared, class_id)
}

#[cold]
#[inline(never)]
fn class_path_package_check_key(shared: &SharedVm, class_id: ClassId) -> Option<(String, String)> {
    let key = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        if class.loader_id != cratonvm_types::ClassLoaderId::Application {
            return None;
        }
        // A HIDDEN class (`Lookup.defineHiddenClass`, a lambda proxy) is not a
        // class-path class: it lives in its lookup class's package, which that
        // class's own define already handled, and HotSpot defines no package
        // for it. Its stored name `p/C/0x<hex>` also has no package at the
        // last `/`: the key read there is a fresh bogus package `p/C` per
        // hidden class, so every hidden class linked paid the package step
        // -- at least a class-path probe for a class file of that name
        // (`class_path_entry_of`, through `define_or_check_class_path_package`)
        // -- per `defineHiddenClass(b, true)`. Behind `CRATONVM_LINK_SKIPS_HIDDEN_PACKAGE` (default on since the i4 wave-89 triage, =0 off;
        // interpreter round i4 wave 89, lane L5; probe
        // `L5W89HiddenClassPackage`, bench `L5W89HiddenInitBench`).
        if class.hidden
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LINK_SKIPS_HIDDEN_PACKAGE")
        {
            return None;
        }
        let (pkg, _) = class.name.rsplit_once('/')?;
        let url = class.code_source.as_ref()?.url.clone()?;
        (pkg.to_string(), url)
    };
    if shared
        .classes
        .class_path_packages_checked
        .lock()
        .contains(&key)
        || !shared.natives.native_methods.jdk_built_packages()
    {
        return None;
    }
    Some(key)
}

/// Run the JDK's `BuiltinClassLoader.defineOrCheckPackage` for the
/// application class-path class `class_id` being linked on `thread`, when it
/// is due ([`class_path_package_check_due`]): HotSpot runs it in
/// `BuiltinClassLoader.defineClass(cn, Resource)` before it defines the
/// class, and the VM defines the class itself, so its link -- the first
/// point that may run Java -- stands in. The package's first class defines
/// it (into the loader's `packages` map, with the JAR's manifest and code
/// source); a class of a sealed package from another code source is the
/// JDK's `SecurityException`, returned for the link to fail with. Interpreter
/// round i2 wave 62, lane L5 (the i58-L5 proposal's next stage;
/// `CRATONVM_CLASS_PATH_PACKAGE_AT_LINK`; probe
/// `L5W62ClassPathPackageDefine`).
fn define_class_path_package_at_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    let Some(key) = class_path_package_check_due(shared, class_id) else {
        return Ok(());
    };
    // Accepted or not applicable (a nested-jar code source, a `Package` handed
    // out outside the map): either way the pair is not asked again. Only a
    // refusal (`Err`) is asked again.
    //
    // gci i18/na: this context is not under `safe_native_call_impl`; a
    // Rust-side allocation on the way into `defineOrCheckPackage` is an
    // `OutOfMemoryError` of the link (asked again, as any refusal, and not
    // recorded as a sealing refusal) instead of the infallible arm's abort.
    let checked = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
        let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
        cratonvm_native_builtins::lang_class::define_or_check_class_path_package(&mut ctx, class_id)
    })
    .map_err(|oom| MethodCallFailed::from(oom.into_runtime_error()))?;
    if checked.is_err() {
        // The only `Err` is the JDK's sealing `SecurityException`: HotSpot
        // never defined a class of this pair, so `findLoadedClass` must not
        // find one ([`class_path_package_refused`]; interpreter round i4 wave
        // 95, lane L5).
        shared
            .classes
            .class_path_packages_refused
            .lock()
            .insert(key);
        return checked.map(|_| ());
    }
    shared
        .classes
        .class_path_packages_checked
        .lock()
        .insert(key);
    Ok(())
}

/// Whether `class_id` is an application class-path class the VM loaded but
/// HotSpot never defined: still unlinked, and its `(package, code source)`
/// pair refused by the JDK's `defineOrCheckPackage` (a sealing violation) at
/// some door. HotSpot's `BuiltinClassLoader.defineClass` throws before
/// `defineClass1`, so its `findLoadedClass` of any class of the pair answers
/// `null`; the VM still holds the class in `Loaded`. Read by the
/// `findLoadedClass` natives (`NativeContext::class_path_package_refused`).
/// `false` at once while no pair was refused (the common case: one lock,
/// one `is_empty`), or with `CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE=0`.
/// Interpreter round i4 wave 95, lane L5
/// (`docs/internal/fixed-bugs/interpreter-i92-L5-unlinked-class-path-class-doors-skip-the-package-step-FIXED-20261127.md`,
/// "Still open"; probe `L5W95FindLoadedSealedRefused`).
pub fn class_path_package_refused(shared: &SharedVm, class_id: ClassId) -> bool {
    if !shared.classes.unlinked_doors_check_package
        || shared.classes.class_path_packages_refused.lock().is_empty()
    {
        return false;
    }
    let key = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return false;
        };
        if class.loader_id != cratonvm_types::ClassLoaderId::Application
            || !matches!(class.state, ClassState::Loaded | ClassState::Verifying)
        {
            return false;
        }
        let Some((pkg, _)) = class.name.rsplit_once('/') else {
            return false;
        };
        let Some(url) = class.code_source.as_ref().and_then(|cs| cs.url.clone()) else {
            return false;
        };
        (pkg.to_string(), url)
    };
    shared
        .classes
        .class_path_packages_refused
        .lock()
        .contains(&key)
}

/// [`class_path_package_refused`] for a JIT compile door, under the
/// caller's class-manager guard, through an array's bottom element class:
/// `true` when compiled code must not bind `class_id` at compile time
/// because a door already had the JDK's `defineOrCheckPackage` refuse its
/// `(package, code source)` pair (a sealing violation) and the class is
/// still unlinked. The compile defers the site to its run-time class helper,
/// which runs the package step (`check_class_path_package_unlinked_at_door`)
/// and throws HotSpot's `SecurityException` as the interpreter's door does;
/// a site bound by id never calls that helper. `false` at once while no pair
/// was refused (one lock, one `is_empty`) or with
/// `CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE=0`. A pair no door has asked yet
/// still binds (the step needs Java, which a compile cannot run).
/// Interpreter round i4 wave 96, lane L5 (`i95-L5-define-door-loose-ends`
/// item 2).
pub fn class_path_package_refused_at_compile(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> bool {
    if !shared.classes.unlinked_doors_check_package
        || shared.classes.class_path_packages_refused.lock().is_empty()
    {
        return false;
    }
    let mut id = class_id;
    for _ in 0..=u8::MAX {
        let Some(class) = cm.get_class(id) else {
            return false;
        };
        if let Some(info) = class.array_info.as_ref() {
            id = info.component_class_id;
            continue;
        }
        if class.loader_id != cratonvm_types::ClassLoaderId::Application
            || !matches!(class.state, ClassState::Loaded | ClassState::Verifying)
        {
            return false;
        }
        let Some((pkg, _)) = class.name.rsplit_once('/') else {
            return false;
        };
        let Some(url) = class.code_source.as_ref().and_then(|cs| cs.url.clone()) else {
            return false;
        };
        return shared
            .classes
            .class_path_packages_refused
            .lock()
            .contains(&(pkg.to_string(), url));
    }
    false
}

/// The package step of [`define_class_path_package_at_link`] for a class a
/// Java door is about to hand out WITHOUT linking it: `Class.forName(n,
/// false, loader)` (`NativeContext::check_class_path_package_unlinked`).
/// HotSpot runs `defineOrCheckPackage` inside `BuiltinClassLoader.defineClass`,
/// so a class of a sealed package from another code source is refused at its
/// LOAD -- `forName(B, false, app)` throws `SecurityException: sealing
/// violation: ...` as `forName(B, true, app)` does -- where the VM, which runs
/// the step at link, answered the class. Only for a class still unlinked
/// (`Loaded`, or parked after a failed link); a refusal leaves it so and is
/// returned, so every later use asks again and is refused again, as HotSpot
/// refuses every define. No check twice for one (package, code source): an
/// accepted pair is recorded as at link. Interpreter round i4 wave 92, lane L5
/// (`i58-L5-proposal-a-class-path-package-is-defined-when-its-first-class-is`;
/// probe `L5W92ForNameSealedNoInit`); `CRATONVM_FORNAME_CHECKS_PACKAGE=0`
/// leaves the step to the link.
pub fn check_class_path_package_unlinked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if !shared.classes.class_path_package_at_link
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FORNAME_CHECKS_PACKAGE")
    {
        return Ok(());
    }
    class_path_package_step_if_unlinked(shared, thread, class_id)
}

/// [`check_class_path_package_unlinked`] for the other doors that hand out an
/// application class-path class without linking it: the `CONSTANT_Class`
/// resolution of `ldc C.class`, `checkcast` / `instanceof` and `anewarray`
/// (`opcodes` / `constants`), and the `--compatible` `ClassLoader.loadClass`
/// natives (`NativeContext::check_class_path_package_at_door`). HotSpot runs
/// `defineOrCheckPackage` inside `BuiltinClassLoader.defineClass`, so every
/// door that loads a class of a sealed package from another code source
/// throws `SecurityException: sealing violation: package <p> is sealed`; the
/// VM defined the class itself and handed out its mirror. An array class is
/// checked through its bottom element class (loading `[Lp/B;` loads `p/B`).
/// A refusal is returned and recorded nowhere (it is not a `LinkageError`),
/// so the next use asks again and is refused again. `ClassRealm::unlinked_doors_check_package`
/// (`CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE`, default on). Interpreter round i4
/// wave 94, lane L5 (`i92-L5-unlinked-class-path-class-doors-skip-the-package-step`;
/// probe `L5W94SealedUnlinkedDoors`).
///
/// GC: runs Java when the step is due (the class unlinked and its
/// `(package, code source)` pair not yet accepted); the callers hold nothing
/// across it that they do not hold across a class resolution, which can run
/// `loadClass`.
pub fn check_class_path_package_unlinked_at_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if !shared.classes.unlinked_doors_check_package || !shared.classes.class_path_package_at_link
    {
        return Ok(());
    }
    // The bottom element class of an array (JVMS §4.4.1: at most 255
    // dimensions), when it is an unlinked application-loader class; one read
    // guard.
    let due = {
        let cm = shared.classes.class_manager.read();
        let mut id = class_id;
        let mut due = None;
        for _ in 0..=u8::MAX {
            let Some(class) = cm.get_class(id) else {
                break;
            };
            match class.array_info.as_ref() {
                Some(info) => id = info.component_class_id,
                None => {
                    if class.loader_id == cratonvm_types::ClassLoaderId::Application
                        && matches!(class.state, ClassState::Loaded | ClassState::Verifying)
                    {
                        due = Some(id);
                    }
                    break;
                }
            }
        }
        due
    };
    match due {
        Some(element) => define_class_path_package_at_link(shared, thread, element),
        None => Ok(()),
    }
}

/// The package step of [`define_class_path_package_at_link`] for `class_id`
/// when it is an application-loader class still unlinked (`Loaded`, or parked
/// after a failed link); nothing for any other class.
fn class_path_package_step_if_unlinked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    let unlinked = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some_and(|c| {
            c.loader_id == cratonvm_types::ClassLoaderId::Application
                && matches!(c.state, ClassState::Loaded | ClassState::Verifying)
        });
    if !unlinked {
        return Ok(());
    }
    define_class_path_package_at_link(shared, thread, class_id)
}

/// The checks of [`link_class_without_java`]: `true` when the link step of
/// [`initialize_class_shared`] would pass for `class_id` without loading
/// anything.
fn link_checks_pass_without_java(shared: &SharedVm, class_id: ClassId) -> bool {
    // The package step runs Java (`define_class_path_package_at_link`).
    if class_path_package_check_due(shared, class_id).is_some() {
        return false;
    }
    // `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK`: this link moves the class to
    // `Verified`, which the Java-thread link then skips, so the walk the
    // define left to the link runs here (it runs no Java); its verifier loads
    // are then judged below like a define-time walk's. One read lock and a
    // probe of an empty table with the switch off.
    publish_pending_type_maps_at_link(shared, class_id);
    if shared.config.skip_verification {
        // The structural scan a user-loader define left to the link still
        // runs (interpreter round i5 wave 103, lane L7).
        let cm = shared.classes.class_manager.read();
        if cm.user_pass3_pending(class_id) {
            if cm.verify_pending_user_structural(class_id).is_err() {
                return false;
            }
            drop(cm);
            shared
                .classes
                .class_manager_write()
                .finish_pending_user_pass3(class_id);
        }
        return true;
    }
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.class_store.get(class_id) else {
        return false;
    };
    if class_link_is_trivial(shared, &cm, class) {
        return true;
    }
    if crate::classloading::verifier::verify_class_structure(class, &cm.class_store).is_err() {
        return false;
    }
    // `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK` (interpreter round i5
    // wave 102, lane L7): the Pass 3 verdict the define left to the link.
    // This link moves the class to `Verified`, which the Java-thread link
    // then skips, so the verdict is given here; a refusal leaves the class
    // unlinked for that link to raise the `VerifyError`.
    if cm.user_pass3_pending(class_id) {
        if cm.verify_pending_user_pass3(class_id).is_err() {
            return false;
        }
        drop(cm);
        shared
            .classes
            .class_manager_write()
            .finish_pending_user_pass3(class_id);
        let cm = shared.classes.class_manager.read();
        return link_checks_after_verdict_pass(shared, &cm, class_id);
    }
    link_checks_after_verdict_pass(shared, &cm, class_id)
}

/// The tail of [`link_checks_pass_without_java`]: the catch-type and verifier
/// loads the link would make, all settled without Java.
fn link_checks_after_verdict_pass(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> bool {
    // The catch-type loads of the link step below, wherever it makes them
    // (`CatchTypesAtLink`; wave 50, lane L5).
    (!shared
        .classes
        .aligned(cratonvm_types::HotSpotAlignment::CatchTypesAtLink)
        || crate::runtime::interpreter::catch_types_settled_without_java(cm, class_id))
        // The verifier's assignability loads (the i47 page) ask the loader.
        && crate::runtime::interpreter::verifier_types_settled_without_java(cm, class_id)
}

/// Did the supertype `super_id`, whose initialization just failed, fail while
/// LINKING? HotSpot links a class's supertypes as part of linking the class
/// itself (`InstanceKlass::link_class_impl`), so that is a link failure of the
/// subtype too, which must stay retryable rather than become erroneous.
///
/// Asked of the supertype's STATE alone, not [`class_link_failed`]: the
/// supertype's initialization has already returned its error, so a supertype
/// still unlinked failed to link, even if another thread has claimed it since
/// to link it again (which `class_link_failed` would read as "not failed",
/// making this class erroneous for good over a retryable link failure).
fn supertype_failed_to_link(shared: &SharedVm, super_id: ClassId) -> bool {
    shared
        .classes
        .class_manager
        .read()
        .get_class(super_id)
        .is_some_and(|c| matches!(c.state, ClassState::Loaded | ClassState::Verifying))
}

/// Is `class` still to be linked by [`link_class_on_thread`] — never linked,
/// parked after a link failure ([`LINK_FAILED_STATE`]), or verified but not
/// prepared ([`link_class_without_java`]) — and claimed by no thread? A class
/// some thread has claimed (this one included: a recursive request) is that
/// claimant's to link.
#[inline]
pub fn class_awaits_link(class: &Class) -> bool {
    matches!(
        class.state,
        ClassState::Loaded | ClassState::Verifying | ClassState::Verified
    ) && class.initializing_thread.is_none()
}

/// Link the direct superclass and the direct superinterfaces of `class_id`
/// that still await linking ([`link_class_on_thread`], which links each one's
/// own supertypes first). HotSpot's `InstanceKlass::link_class_impl` does this
/// before it links the class itself, so a supertype's `ClassPrepare` precedes
/// its subtype's and a class whose supertype fails to link is never prepared.
fn link_supertypes_on_thread(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    let pending: Vec<ClassId> = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(());
        };
        class
            .superclass
            .into_iter()
            .chain(class.interfaces.iter().copied())
            .filter(|&id| cm.get_class(id).is_some_and(class_awaits_link))
            .collect()
    };
    for super_id in pending {
        link_class_on_thread(shared, thread, super_id)?;
    }
    Ok(())
}

/// Link `class_id` on this thread without initializing it: HotSpot's
/// `InstanceKlass::link_class_impl` (interpreter round i1 wave 17). The
/// supertypes first ([`link_supertypes_on_thread`]), then this class's
/// verification and preparation ([`link_claimed_class`]), which reports
/// `ClassPrepare`; the class is left `Prepared`, for its initialization to
/// skip both steps.
///
/// The class is claimed the way initialization claims it
/// (`initializing_thread` = this thread, plus a `class_init_waiters` entry,
/// under the class-manager write lock), so a thread that asks to initialize it
/// meanwhile waits for the link and then initializes it. A class already
/// linked, claimed by any thread (this one: a recursive request; another: that
/// thread links it), or still `Loading` is left alone: this never waits, so it
/// cannot deadlock against a `<clinit>` running on another thread.
///
/// A failure parks the class unlinked ([`LINK_FAILED_STATE`], retryable, as a
/// link failure inside initialization does) and is returned: the supertype's
/// linkage error, a `VerifyError`, a catch-type load failure under
/// `--jdk-only`, or preparation running out of memory.
pub fn link_class_on_thread(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    let awaits = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some_and(class_awaits_link);
    if !awaits {
        return Ok(());
    }
    link_supertypes_on_thread(shared, thread, class_id)?;
    let claimed = {
        let mut cm = shared.classes.class_manager_write();
        match cm.get_class_mut(class_id).filter(|c| class_awaits_link(c)) {
            Some(class) => {
                class.initializing_thread = Some(thread.thread_id.0);
                register_init_waiter(shared, class_id);
                true
            }
            None => false,
        }
    };
    if !claimed {
        return Ok(());
    }
    let claimant = thread.thread_id.0;
    let held = {
        let finalized = std::cell::Cell::new(false);
        let failure_state = std::cell::Cell::new(LINK_FAILED_STATE);
        let _link_guard = InitCleanupGuard {
            shared,
            class_id,
            finalized: &finalized,
            failure_state: &failure_state,
            claimant,
        };
        link_claimed_class(shared, thread, class_id, None)?;
        finalized.set(true);
        finalize_class_init(shared, class_id, ClassState::Prepared, claimant)
    };
    if !held {
        // The claim linked a stub that was upgraded meanwhile: link the real
        // class (a class is upgraded once, so this recurs at most once).
        return link_class_on_thread(shared, thread, class_id);
    }
    Ok(())
}

/// Kill switch of [`reflection_links_class_enforced`] (interpreter round i1
/// wave 17): `true` since its `[reflect-link-census]` census read no
/// `checks=fails` line — under `--jdk-only` over the core suite and a Spring
/// Boot application (wave 17), and under `--compatible` over those plus 40
/// Tomcat test classes and two Netty suites (wave 18, 1,863 lines);
/// `docs/internal/fixed-bugs/interpreter-L1-linking-happens-only-inside-initialization-FIXED-20260925.md`.
const REFLECTION_LINKS_CLASS: bool = true;

/// Does reflection link the class it answers for, as HotSpot's
/// `JVM_GetClassDeclaredMethods` / `Fields` / `Constructors` do
/// (`link_class` first, so the class is verified, prepared and reported to
/// `ClassPrepare` listeners, and a class that fails to link throws its
/// linkage error from the reflective call)? The one policy point: every mode
/// since wave 18.
#[inline]
pub fn reflection_links_class_enforced(_shared: &SharedVm) -> bool {
    REFLECTION_LINKS_CLASS
}

/// `Class.getDeclaredMethods0` / `getDeclaredFields0` /
/// `getDeclaredConstructors0` ask this before answering for `class_id`
/// (`NativeContext::link_class_for_reflection`). Where
/// [`reflection_links_class_enforced`] holds it links the class
/// ([`link_class_on_thread`]) and returns its linkage error. Elsewhere it
/// does nothing, except under `CRATONVM_DBG_LINKAGE`, where it prints one
/// `[reflect-link-census]` line for a class the policy would link now, with
/// the verdict of its link-time structural check (`trivial`, `passes`,
/// `fails`). Unarmed and unenforced: one cached flag load.
///
/// gci i22/oo (`gcg-g4oo-oome-ladder-review-findings` item 5): a link whose
/// preparation the heap refused ([`prepare_class_shared`]'s `ConstantValue`
/// Strings) collects once and links again
/// ([`link_class_for_reflection_reclaiming`]).
pub fn link_class_for_reflection(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if reflection_links_class_enforced(shared) {
        return link_class_for_reflection_reclaiming(shared, thread, class_id);
    }
    if !crate::runtime::interpreter::dbg_linkage() {
        return Ok(());
    }
    let line = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(());
        };
        if !class_awaits_link(class) {
            return Ok(());
        }
        let verdict = if class_link_is_trivial(shared, &cm, class) {
            "trivial"
        } else if crate::classloading::verifier::verify_class_structure(class, &cm.class_store)
            .is_err()
        {
            "fails"
        } else {
            "passes"
        };
        format!(
            "[reflect-link-census] class={} state={} checks={verdict} mode={}",
            class.name,
            class.state,
            if shared.config.is_jdk_only() {
                "jdk-only"
            } else {
                "compatible"
            }
        )
    };
    eprintln!("{line}");
    Ok(())
}

/// gci i22/oo (`gcg-g4oo-oome-ladder-review-findings` item 5): the reflective
/// link, with one collection and one more link when the heap refused it.
///
/// HotSpot builds a class's `ConstantValue` Strings with its mirror, at load,
/// through a collecting allocation, so `getDeclaredFields()` (and Methods,
/// Constructors) of a loaded class never allocates them; CratonVM builds them
/// at preparation ([`prepare_class_shared`]), which never collects, and this
/// link-only path (unlike initialization, `link_claimed_class`'s g1b/o
/// reclaim) had no retry: a heap full of garbage threw
/// `OutOfMemoryError: Java heap space (prepare_class ... ConstantValue String)`
/// where HotSpot answers.
///
/// Collecting here is safe because of the callers, not of this function: the
/// three reflective natives (`native-builtins` `lang_class.rs`
/// `getDeclaredFields0` / `getDeclaredMethods0` / `getDeclaredConstructors0`)
/// call `NativeContext::link_class_for_reflection` before they hold any heap
/// reference they read again (their receiver mirror is not read after it), the
/// precondition `reclaim_before_alloc_retry` states; the unit test
/// `gci_i22oo_the_reflective_link_callers_hold_no_reference_across_it` pins
/// that there are exactly three. A failed link is parked unlinked and
/// retryable (`LINK_FAILED_STATE`), so the second link starts over; nothing
/// the first attempt built is held across the collection. Only a heap refusal
/// collects (`prep_refusal_may_reclaim`, the same rule and the same
/// `CRATONVM_GC_CLASS_PREP_RECLAIM=0` kill as the initialization path).
fn link_class_for_reflection_reclaiming(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    use crate::runtime::interpreter::gc_and_alloc as ladder;
    let err = match link_class_on_thread(shared, thread, class_id) {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };
    if !reflection_link_refusal_may_reclaim(&err) {
        return Err(err);
    }
    let verdict = ladder::native_reclaim_before_alloc_retry_graced(shared, thread);
    if !verdict.retry {
        return Err(err);
    }
    let mut relinked = link_class_on_thread(shared, thread, class_id);
    // zr z8/nr: as the initialization path's preparation reclaim.
    if relinked
        .as_ref()
        .err()
        .is_some_and(reflection_link_refusal_may_reclaim)
    {
        if ladder::zgc_native_repay_skipped_soft_rung(shared, thread) {
            relinked = link_class_on_thread(shared, thread, class_id);
        }
    } else if relinked.is_ok() {
        ladder::zgc_native_room_skip_served(shared, thread);
    }
    if verdict.graced {
        ladder::note_latched_grace_outcome(shared, relinked.is_ok());
    }
    relinked
}

/// The rule of [`link_class_for_reflection_reclaiming`]: a heap refusal
/// (`VmError::Runtime(OutOfMemoryError)`, what `prepare_class_shared` answers)
/// under `CRATONVM_GC_CLASS_PREP_RECLAIM`. A Java `OutOfMemoryError` thrown by
/// Java the link ran (`MethodCallFailed::ExceptionThrown`) already had its
/// collections; a linkage error is not the heap's.
fn reflection_link_refusal_may_reclaim(err: &MethodCallFailed) -> bool {
    matches!(err, MethodCallFailed::InternalError(e) if prep_refusal_may_reclaim(e, true))
}

/// `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK` (interpreter round i3 wave 81, lane
/// L5;
/// `docs/internal/fixed-bugs/interpreter-i72-L5-proposal-define-time-type-maps-at-link-DONE-20261115.md`):
/// run the type-map walk a user-loader class's define left to its link. The
/// walk runs under the class-manager READ lock (it reads the store and
/// publishes into the lock-free `TypeMapStore`; the define ran it under the
/// WRITE lock); the verifier loads it recorded are filed under a short write
/// lock for the link's verifier-load step that follows.
#[cold]
#[inline(never)]
fn publish_pending_type_maps_at_link(shared: &SharedVm, class_id: ClassId) {
    let loads = shared
        .classes
        .class_manager
        .read()
        .publish_pending_type_maps(class_id);
    if let Some(loads) = loads {
        shared
            .classes
            .class_manager_write()
            .finish_pending_type_maps(class_id, loads);
    }
}

/// Link a class this thread has claimed (`initializing_thread`): Step 1,
/// structural verification (Loaded -> Verifying -> Verified) and, under
/// `--jdk-only`, the catch-type loads; Step 2, preparation (Verified ->
/// Preparing -> Prepared) and its `ClassPrepare` report. A step the class is
/// already past is skipped, so a class [`link_class_on_thread`] linked earlier
/// is neither verified nor prepared again (a second preparation would replace
/// its statics block).
///
/// `failure_state` is the initialization guard's ([`InitCleanupGuard`]): it
/// is switched to `InitializationError` once verification has passed, as
/// before wave 17. [`link_class_on_thread`] passes `None` and parks every
/// failure unlinked.
fn link_claimed_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    failure_state: Option<&std::cell::Cell<ClassState>>,
) -> Result<(), MethodCallFailed> {
    // `CRATONVM_DBG=mh-finder-census` (wave 90, lane L4): this link's time by
    // scope. One flag read when the census is off. Wave 94: named by class
    // inside a census scope (`class-link.<scope> <class>`); no lock is held
    // here (the write lock below is taken after).
    let _census_clock = cratonvm_native_builtins::mh_finder_census::work_clock_named(
        cratonvm_native_builtins::mh_finder_census::Work::ClassLink,
        || census_class_name(shared, class_id),
    );
    // `CRATONVM_DBG_DEFINE_CENSUS` (wave 90, lane L5): taken first, so an
    // early return below never leaves the arm for the next link.
    let census_link = cratonvm_classloading::define_census::take_define_link();
    // Step 1: Structural verification (Loaded -> Verifying -> Verified).
    //
    // Linking precedes the superclass's `<clinit>`, as in HotSpot
    // (`initialize_impl` calls `link_class` before JVMS §5.5 step 7): a class
    // that fails to link must not have run its superclass's initializer.
    //
    // Every state write below is made only while this thread's claim still
    // holds the class (`init_claim_held`): a stub upgraded in place meanwhile
    // (`--compatible`) is back to `Loaded` with no claim, and is the next
    // claim's to link. A lost claim returns `Ok` at once; its caller's
    // finalize sees the loss and retries (wave 62, lane L5).
    let claimant = thread.thread_id.0;
    let (linking_now, type_maps_pending) = {
        let mut cm = shared.classes.class_manager_write();
        let linking_now = match cm.get_class_mut(class_id) {
            Some(class) if !init_claim_held(class, claimant) => return Ok(()),
            Some(class) => {
                if class.state == ClassState::Loaded {
                    class.state = ClassState::Verifying;
                }
                class.state == ClassState::Verifying
            }
            None => false,
        };
        // `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK`: the define left this class's
        // type-map walk to here (an empty-table probe with the switch off).
        (linking_now, linking_now && cm.type_maps_pending(class_id))
    };
    // HotSpot's `BuiltinClassLoader.defineClass` defines (or checks) an
    // application class-path class's package before the class: here, the
    // first point that may run Java. A sealing violation is a link failure of
    // the class (retried, and refused again, by its next use).
    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 90, lane L5):
    // the `link-*` rows time the link of a class a define initializes (armed
    // by `initialize_class_shared`); `false` and no clock otherwise.
    // (`census_link` is taken at the top of this function.)
    use cratonvm_classloading::define_census::{phase_end, phase_guard, phase_start_if, DefinePhase};
    let link_clock = phase_start_if(census_link);
    if linking_now {
        define_class_path_package_at_link(shared, thread, class_id)?;
    }
    phase_end(DefinePhase::LinkPackage, link_clock);
    let link_clock = phase_start_if(census_link);
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
    //
    // `load_catch_types` is set when this class passed the link-time verification below and, under
    // `--jdk-only`, still owes HotSpot's catch-type loading (run after the
    // class-manager read guard is released: it may run a Java `loadClass`).
    let mut load_catch_types = false;
    // The pending Pass 3 verdict passed (settled below, under the write lock).
    let mut user_pass3_passed = false;
    // The verifier loads of the link's single walk, when it ran
    // (`CRATONVM_USER_PASS3_RECORDS_LINK_LOADS`; settled with the verdict).
    let mut one_walk_loads: Option<Box<[crate::classloading::vtype::VerifierLinkLoad]>> = None;
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
                    //
                    // The one exception is a user-loader class whose define
                    // left its Pass 3 verdict here
                    // (`CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK`;
                    // interpreter round i5 wave 102, lane L7): HotSpot
                    // verifies at link, so a class defined and never linked
                    // raises nothing. It runs through the define's
                    // loader-aware hierarchy, not this name-indexed store
                    // (`ClassManager::verify_pending_user_pass3`).
                    //
                    // `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS` (default off;
                    // interpreter round i6 wave 119, lane L5): a class that also
                    // owes the type-map walk gets ONE walk, the verdict's, which
                    // records the verifier loads and publishes the maps; the
                    // type-map step below then finds nothing pending.
                    let one_walk = cm.user_pass3_one_walk_due(class_id);
                    let bytecode = structural.and_then(|()| {
                        if one_walk {
                            cm.verify_pending_user_pass3_one_walk(class_id)
                                .map(|loads| one_walk_loads = Some(loads))
                        } else {
                            cm.verify_pending_user_pass3(class_id)
                        }
                    });
                    user_pass3_passed = bytecode.is_ok() && cm.user_pass3_pending(class_id);
                    if let Err(e) = bytecode {
                        drop(cm);
                        // HotSpot checks a method's handler table, LOADING
                        // each catch type, before its code
                        // (`verify_exception_handler_table`): a missing or
                        // non-`Throwable` catch type is the error it reports,
                        // not the type error found after it. Pass 3 cannot
                        // load (it runs under the class manager), so a refused
                        // walk asks the catch-type step first (interpreter
                        // round i5 wave 104, lane L7, `i101-L7-verifyerror-
                        // texts-are-not-hotspots`).
                        if matches!(e, LinkageError::VerifyError { .. })
                            && shared
                                .classes
                                .aligned(cratonvm_types::HotSpotAlignment::CatchTypesAtLink)
                        {
                            crate::runtime::interpreter::load_catch_types_at_link(
                                shared, thread, class_id,
                            )?;
                        }
                        // Cleanup (park the class unlinked -- a link failure,
                        // retried by the next use -- clear the init claim, remove
                        // the waiter, notify all waiters) is run by
                        // `InitCleanupGuard::drop` since `finalized` is still
                        // false on this early return. Funnelling through the guard
                        // keeps a single cleanup point and also releases threads
                        // blocked on this class.
                        return Err(MethodCallFailed::InternalError(VmError::Linkage(e)));
                    }
                    // `--compatible` too with the `CatchTypesAtLink` row
                    // (interpreter round i1 wave 50, lane L5; probe
                    // `CatchTypeLinkProbe`): that mode answered the missing
                    // catch type at the first throw reaching the handler.
                    load_catch_types = shared
                        .classes
                        .aligned(cratonvm_types::HotSpotAlignment::CatchTypesAtLink);
                }
            }
        }
    } else if linking_now {
        // `--noverify`: no verifier, as on HotSpot, but the structural scan a
        // user-loader define left to this link still runs: the interpreter
        // relies on it (interpreter round i5 wave 103, lane L7).
        let cm = shared.classes.class_manager.read();
        if cm.user_pass3_pending(class_id) {
            if let Err(e) = cm.verify_pending_user_structural(class_id) {
                drop(cm);
                return Err(MethodCallFailed::InternalError(VmError::Linkage(e)));
            }
            user_pass3_passed = true;
        }
    }
    if user_pass3_passed {
        let mut cm = shared.classes.class_manager_write();
        match one_walk_loads.take() {
            // The single walk settles the type-map row too.
            Some(loads) => cm.finish_user_pass3_one_walk(class_id, loads),
            None => cm.finish_pending_user_pass3(class_id),
        }
    }
    phase_end(DefinePhase::LinkStructure, link_clock);
    let link_clock = phase_start_if(census_link);
    // `CRATONVM_DEFINE_TYPE_MAPS_AT_LINK` (interpreter round i3 wave 81, lane
    // L5): the type maps (and the verifier loads below) of a class whose
    // define left them to its link, before any of its code can run.
    if type_maps_pending {
        publish_pending_type_maps_at_link(shared, class_id);
    }
    // The verifier's loads (the i47 page): the names HotSpot's verifier loads
    // through the class's loader -- each method's handler table (a catch type
    // and `Throwable`), then its reference assignability checks -- in that
    // order, before the catch-type step below judges each catch type. A class
    // owes them only when its define-time verification recorded them, which
    // it does wherever the `VerifierLoadsAtLink` alignment is on.
    if linking_now && !shared.config.skip_verification {
        if let Err(e) =
            crate::runtime::interpreter::load_verifier_types_at_link(shared, thread, class_id)
        {
            shared
                .classes
                .note_alignment_divergence(cratonvm_types::HotSpotAlignment::VerifierLoadsAtLink);
            return Err(e);
        }
    }
    phase_end(DefinePhase::LinkVerifierLoads, link_clock);
    let link_clock = phase_start_if(census_link);
    if load_catch_types {
        // A failure is a linkage error of this class, cleaned up by
        // `InitCleanupGuard` like the Pass 2 failure above.
        if let Err(e) =
            crate::runtime::interpreter::load_catch_types_at_link(shared, thread, class_id)
        {
            // The row-off route linked the class and met the missing type at
            // the first throw reaching the handler, if ever (wave 51, lane
            // L5: the alignment census).
            shared
                .classes
                .note_alignment_divergence(cratonvm_types::HotSpotAlignment::CatchTypesAtLink);
            return Err(e);
        }
    }
    phase_end(DefinePhase::LinkCatchTypes, link_clock);
    let link_clock = phase_start_if(census_link);
    // JVMS §5.3.4 for overrides across loaders, as HotSpot's vtable build
    // checks it while linking (interpreter round i1 wave 39, lane L5). A
    // failure is a linkage error of this class, cleaned up by
    // `InitCleanupGuard` like the Pass 2 failure above. Wherever constraints
    // are kept (`ClassRealm::loader_constraints_on`: `--jdk-only`, or
    // `--compatible` with the `LoaderConstraints` row): wave 49 moved every
    // gate inside `loader_constraints` to that answer but left this caller
    // on `is_jdk_only`, so `--compatible` linked every override and itable
    // violation (interpreter round i1 wave 50, lane L5; probes
    // `L5W37LoaderConstraintOverride`, `L5W39OverrideConstraintPending`,
    // `L5W40ItableConstraint`, `L5W42ConstraintMessageNames`).
    if linking_now && shared.classes.loader_constraints_on {
        crate::runtime::resolve::loader_constraints::check_override_constraints_at_link(
            shared, class_id,
        )?;
    }
    phase_end(DefinePhase::LinkConstraints, link_clock);
    // Everything from here to the end (state writes, preparation,
    // `ClassPrepare`, the init-path mirror), on every exit.
    let _census_prepare = phase_guard(DefinePhase::LinkPrepare, phase_start_if(census_link));
    // `link-prepare` split (interpreter round i4 wave 93, lane L5):
    // `prepare-states` (the `Verified` / `Preparing` writes), `prepare-statics`
    // (`prepare_class_shared`, with its reclaim retry), `prepare-tail` (the
    // `Prepared` write and `ClassPrepare`) and `prepare-mirror` (the init-path
    // mirror: for a define that initializes, `defineHiddenClass(b, true)`, the
    // class's mirror is MADE here, before the native's own mirror step).
    let prepare_clock = phase_start_if(census_link);
    // `CRATONVM_LINK_STATE_ONE_WRITE` (interpreter round i4 wave 91, lane L5;
    // `ClassRealm::link_state_one_write`, default off): the `Verified` write
    // and the `Preparing` write below under ONE class-manager write guard.
    // Same states, same claim checks; the `Verified` state is simply never
    // published on its own (nothing waits on it, and every "linked?" reader
    // answers the same for `Verified` and `Preparing`). One write acquisition
    // and one guard release (with its deferred-`new` re-sweep) fewer per link.
    let prepare_now = if shared.classes.link_state_one_write {
        let prepare_now = {
            let mut cm = shared.classes.class_manager_write();
            let prepare_now = match cm.get_class_mut(class_id) {
                Some(class) if !init_claim_held(class, claimant) => return Ok(()),
                Some(class) => {
                    if class.state == ClassState::Verifying {
                        class.state = ClassState::Verified;
                    }
                    if class.state == ClassState::Verified {
                        class.state = ClassState::Preparing;
                        true
                    } else {
                        false
                    }
                }
                // No such class: `prepare_class_shared` refuses it.
                None => true,
            };
            // `Verified` reads as linked (`class_linked_or_link_trivial`).
            crate::runtime::interpreter::jit_bridge::note_new_site_settle_event(shared);
            prepare_now
        };
        // Linked (see the switch-off arm below).
        if let Some(failure_state) = failure_state {
            failure_state.set(ClassState::InitializationError);
        }
        prepare_now
    } else {
        {
            let mut cm = shared.classes.class_manager_write();
            if let Some(class) = cm.get_class_mut(class_id) {
                if !init_claim_held(class, claimant) {
                    return Ok(());
                }
                if class.state == ClassState::Verifying {
                    class.state = ClassState::Verified;
                }
            }
            // `Verified` reads as linked (`class_linked_or_link_trivial`).
            crate::runtime::interpreter::jit_bridge::note_new_site_settle_event(shared);
        }
        // Linked: from here on a failure makes the class erroneous (JVMS §5.5
        // steps 7 and 11) on the initialization path.
        if let Some(failure_state) = failure_state {
            failure_state.set(ClassState::InitializationError);
        }

        // Step 2: Preparation (Verified -> Preparing -> Prepared), part of
        // linking (JVMS §5.4.2), so BEFORE the superclass is initialized, as in
        // HotSpot (`link_class_impl` prepares and posts `ClassPrepare` before
        // `initialize_impl` reaches step 7). Until wave 15 it followed the
        // superclass's `<clinit>`, and a store that `<clinit>` made into this
        // class's statics (a recursive-initialization request, JVMS §5.5 step 3,
        // then `putstatic`) landed in a block this step then replaced: lost.
        // A class already prepared (by `link_class_on_thread`) keeps its block.
        {
            let mut cm = shared.classes.class_manager_write();
            match cm.get_class_mut(class_id) {
                Some(class) if !init_claim_held(class, claimant) => return Ok(()),
                Some(class) if class.state == ClassState::Verified => {
                    class.state = ClassState::Preparing;
                    true
                }
                Some(_) => false,
                // No such class: `prepare_class_shared` refuses it, as it did
                // before this step learned to skip a class already prepared.
                None => true,
            }
        }
    };
    phase_end(DefinePhase::PrepareStates, prepare_clock);
    if !prepare_now {
        // gci i22/oo (`gcg-g4oo-oome-ladder-review-findings` item 4): a class
        // prepared EARLIER (reflection's link, the VM's exception link,
        // `link_class_on_thread`) skipped the init-path mirror below, so its
        // mirror stayed lazy -- minted later by a native that may not collect.
        // It is made here on the initialization path too, under the same
        // switch (a cached mirror is one read-lock hit).
        init_path_mirror_for_prepared_class(shared, thread, class_id, failure_state.is_some());
        return Ok(());
    }
    let prepare_clock = phase_start_if(census_link);
    if let Err(err) = prepare_class_shared(shared, class_id) {
        // gcg g1b/o (`gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors`,
        // door 1): a heap refusal of a `ConstantValue` String gets one
        // collection and one retry of the whole preparation, as HotSpot's
        // mirror creation (which builds those Strings) collects. Only on the
        // INITIALIZATION path (`failure_state` set): it runs the class's
        // superclass `<clinit>` and its own right after this step, so its
        // callers already allow a collection here. The retry rebuilds every
        // constant, so nothing the failed attempt made is held across the
        // collection. `CRATONVM_GC_CLASS_PREP_RECLAIM=0` restores the error.
        if !prep_refusal_may_reclaim(&err, failure_state.is_some()) {
            return Err(err.into());
        }
        // gcg g4/oo (`gcg-g3o-init-path-reclaim-may-collect-under-a-native-holding-unpinned-refs`):
        // a native that initializes a class through
        // `NativeContext::ensure_class_initialized` holds unpinned references
        // in Rust locals. It already allows a collection whenever a
        // `<clinit>` runs, but not when none does; collect only then.
        if !init_path_may_collect(shared, class_id) {
            return Err(err.into());
        }
        let verdict =
            crate::runtime::interpreter::gc_and_alloc::native_reclaim_before_alloc_retry_graced(
                shared, thread,
            );
        if !verdict.retry {
            return Err(err.into());
        }
        let mut retried = prepare_class_shared(shared, class_id);
        // zr z8/nr: a failed retry pays a skipped ZGC clear-soft rung (the
        // ladder's collection had made room that was taken before the retry),
        // then retries once more; a served retry releases the skip.
        if retried.is_err() {
            if crate::runtime::interpreter::gc_and_alloc::zgc_native_repay_skipped_soft_rung(
                shared, thread,
            ) {
                retried = prepare_class_shared(shared, class_id);
            }
        } else {
            crate::runtime::interpreter::gc_and_alloc::zgc_native_room_skip_served(shared, thread);
        }
        // gcg g3/o: this door makes its own retry, so it reports a graced
        // retry's outcome itself; nothing else would (the factory door's
        // pending-retry slot is counted by a `NativeContextImpl` allocation).
        if verdict.graced {
            crate::runtime::interpreter::gc_and_alloc::note_latched_grace_outcome(
                shared,
                retried.is_ok(),
            );
        }
        retried?;
    }
    phase_end(DefinePhase::PrepareStatics, prepare_clock);
    let prepare_clock = phase_start_if(census_link);
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            if !init_claim_held(class, claimant) {
                return Ok(());
            }
            if class.state == ClassState::Preparing {
                class.state = ClassState::Prepared;
            }
        }
        // Linked and prepared: see `finalize_class_init`'s settle event.
        crate::runtime::interpreter::jit_bridge::note_new_site_settle_event(shared);
    }
    // `ClassPrepare` (JDWP and a C agent's) from the preparing thread
    // (interpreter round i1 wave 10, lane L1): reported, and its policy
    // applied to this thread, before any code of the class or of its
    // superclass runs (wave 15). Once per class: a retry after a link failure
    // that raced a supertype's link on another thread prepares again, and the
    // consumers deduplicate. One load with no request in force.
    //
    // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    // item 1c): with this thread's JNI binding installed from `thread`
    // itself, so a C agent's `ClassPrepare` callback (and a nested
    // `ClassLoad` its JNI calls cause) reborrows this handle, not the
    // registry's address of the same thread. Two thread-local writes per
    // prepared class.
    #[cfg(feature = "experimental-debug")]
    {
        let tid = thread.thread_id.0;
        crate::runtime::interpreter::post_on_own_thread(thread, || {
            // A `ClassLoad` deferred on this thread (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`)
            // comes before any `ClassPrepare`, as in HotSpot.
            crate::jvmti::native_env::flush_deferred_class_loads(shared, tid);
            crate::debug::class_prepared_on_thread(shared, tid, class_id)
        });
    }
    // The VM's own Rust-side JVMTI manager, at the same point (wave 17; it
    // used to be told at definition time). One load with no listener.
    crate::runtime::jvmti::fire_class_prepare_for_vm(
        shared.vm_identity,
        thread.thread_id.0,
        u64::from(class_id.as_u32()),
    );
    phase_end(DefinePhase::PrepareTail, prepare_clock);
    let prepare_clock = phase_start_if(census_link);
    // gcg g3/o (opt-in, `gcg-g3o-proposal-class-mirror-created-on-the-init-path`):
    // the mirror now, where a refusal may collect, instead of at its first
    // native use, where it may not (door 2 of
    // `gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors`).
    if failure_state.is_some() && mirror_at_init_enabled(shared) {
        let _ = create_mirror_on_init_path(shared, thread, class_id);
    }
    phase_end(DefinePhase::PrepareMirror, prepare_clock);
    Ok(())
}

/// gcg g3/o: `CRATONVM_GC_MIRROR_AT_INIT` (default ON since gcg g6/oo,
/// `runtime_flag_default_on`; see [`mirror_at_init_flag`]).
/// gcg g6/oo: read once per VM (`AllocLadderState::mirror_at_init`): this is
/// asked on every class initialization.
fn mirror_at_init_enabled(shared: &SharedVm) -> bool {
    *shared
        .mem
        .alloc_ladder
        .mirror_at_init
        .get_or_init(mirror_at_init_flag)
}

/// The flag read behind [`mirror_at_init_enabled`]. gcg g6/oo: default ON
/// (`runtime_flag_default_on`, `=0` restores the lazy first-use mirror): vg3
/// ZGC `mi` printed the probe's lines where the default died, G1 equal; vg5
/// `oo5_mi_cost_*` wall within 3 % over 4 pairs. HotSpot has every mirror from
/// class load.
fn mirror_at_init_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_MIRROR_AT_INIT")
}

/// gcg g3/o: give `class_id` its `java.lang.Class` mirror on the
/// initialization path (`link_claimed_class` with `failure_state` set, which
/// runs the superclass's `<clinit>` and the class's own right after, so its
/// callers already allow a collection). A refusal collects once through the
/// native factory door and retries; a second refusal is not an error -- the
/// mirror keeps its lazy first use. HotSpot creates every mirror at class
/// load (`java_lang_Class::create_mirror`), with a collecting allocation, so
/// a later `getClass()` inside a native never allocates. Answers whether the
/// class ended with a mirror.
fn create_mirror_on_init_path(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> bool {
    use crate::runtime::interpreter::gc_and_alloc as ladder;
    if crate::vm::try_get_or_create_class_mirror(shared, class_id).is_some() {
        return true;
    }
    // gcg g4/oo: the same guard as the preparation reclaim above.
    if !init_path_may_collect(shared, class_id) {
        return false;
    }
    let verdict = ladder::native_reclaim_before_alloc_retry_graced(shared, thread);
    if !verdict.retry {
        return false;
    }
    let mut made = crate::vm::try_get_or_create_class_mirror(shared, class_id).is_some();
    // zr z8/nr: see the preparation reclaim above.
    if !made {
        if ladder::zgc_native_repay_skipped_soft_rung(shared, thread) {
            made = crate::vm::try_get_or_create_class_mirror(shared, class_id).is_some();
        }
    } else {
        ladder::zgc_native_room_skip_served(shared, thread);
    }
    if verdict.graced {
        // This door makes its own retry, so it reports the grace's outcome
        // itself (`gcf-f1o-latched-grace-ok-is-counted-by-one-door-of-five`).
        ladder::note_latched_grace_outcome(shared, made);
    }
    made
}

/// gci i22/oo (g4oo item 4): the init-path mirror of a class
/// `link_claimed_class` found already prepared. `initializing` is
/// `failure_state.is_some()` (the initialization path, which may collect;
/// [`create_mirror_on_init_path`] still asks [`init_path_may_collect`]). The
/// same switch as the prepared-here arm (`CRATONVM_GC_MIRROR_AT_INIT`).
fn init_path_mirror_for_prepared_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    initializing: bool,
) {
    if initializing && mirror_at_init_enabled(shared) {
        let _ = create_mirror_on_init_path(shared, thread, class_id);
    }
}

/// gcg g4/oo, `gcg-g3o-init-path-reclaim-may-collect-under-a-native-holding-unpinned-refs`:
/// may the initialization path of `class_id` collect (the g1b preparation
/// reclaim, the opt-in init-path mirror)?
///
/// Yes when no native callback is the nearest frame
/// (`native_oom::unwind_ok()` is false: interpreted or compiled Java, the
/// boot path), or when this initialization runs Java anyway (the class or a
/// superclass not yet initialized declares a bytecode `<clinit>`), so the
/// caller already allows a collection here. Otherwise a native reached the
/// initialization through `NativeContext::ensure_class_initialized` for a
/// class with no `<clinit>` to run, and may hold unpinned `ObjectRef`s in Rust
/// locals: the refusal keeps its error, as before g1b.
///
/// `unwind_ok()` is also raised for interpreted Java a native calls back
/// into; there the answer is conservative (no collection without a
/// `<clinit>`), never unsafe.
fn init_path_may_collect(shared: &SharedVm, class_id: ClassId) -> bool {
    let in_native = crate::runtime::native_oom::unwind_ok();
    // Asked only when it decides: one class-manager read per refusal.
    init_path_collect_safe(in_native, || class_init_runs_java(shared, class_id))
}

/// The pure rule of [`init_path_may_collect`].
fn init_path_collect_safe(in_native: bool, runs_java: impl FnOnce() -> bool) -> bool {
    !in_native || runs_java()
}

/// Does initializing `class_id` run Java: does it, or a superclass not yet
/// initialized, declare a bytecode `<clinit>`? A native-registered `<clinit>`
/// is Rust and does not count. Superinterfaces are not walked (they are
/// initialized only for default methods), which errs toward "no Java", the
/// safe answer here.
fn class_init_runs_java(shared: &SharedVm, class_id: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let mut next = Some(class_id);
    // Bounded: a malformed hierarchy cannot loop forever.
    for _ in 0..256 {
        let Some(id) = next else {
            return false;
        };
        let Some(class) = cm.get_class(id) else {
            return false;
        };
        if id != class_id
            && matches!(
                class.state,
                ClassState::Initializing
                    | ClassState::Initialized
                    | ClassState::InitializationError
            )
        {
            // Initialized (or failing) superclasses run nothing more.
            return false;
        }
        if class.find_method("<clinit>", "()V").is_some() {
            return true;
        }
        next = class.superclass;
    }
    false
}

/// The class name the finder census tallies a class initialization or link
/// under (`CRATONVM_DBG=mh-finder-census`, interpreter round i4 wave 94, lane
/// L4). Asked only with the census on and inside a census scope; takes the
/// class-manager read lock, which neither caller holds at that point.
#[cold]
fn census_class_name(shared: &SharedVm, class_id: ClassId) -> Option<String> {
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.to_string())
}

fn initialize_class_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    // `CRATONVM_DBG=mh-finder-census` (interpreter round i4 wave 90, lane L4):
    // this initialization's time, charged to the finder / first-invoke scope
    // it ran in. One flag read when the census is off; reads nothing else.
    // Wave 94: named by class inside a census scope
    // (`class-init.<scope> <class>`).
    let _census_clock = cratonvm_native_builtins::mh_finder_census::work_clock_named(
        cratonvm_native_builtins::mh_finder_census::Work::ClassInit,
        || census_class_name(shared, class_id),
    );
    // Cleanup safety net: the caller (`ensure_class_initialized_shared`) has
    // already claimed this class (`initializing_thread` set) and registered a
    // waiter. Every return path below MUST clear that claim and notify waiters,
    // otherwise concurrent threads block until their wait timeout. The terminal
    // success/error paths do so explicitly via the `finalize_init` closure; this
    // guard covers the early `?`/`return Err(..)` paths (superclass init,
    // verification, preparation, superinterface init, `System` stdin) and any
    // panic unwinding through the body.
    let finalized = std::cell::Cell::new(false);
    // Linking comes first and a link failure parks the class unlinked
    // (`LINK_FAILED_STATE`, see `InitCleanupGuard`); switched to
    // `InitializationError` once linked.
    let failure_state = std::cell::Cell::new(LINK_FAILED_STATE);
    let _init_guard = InitCleanupGuard {
        shared,
        class_id,
        finalized: &finalized,
        failure_state: &failure_state,
        claimant: thread.thread_id.0,
    };

    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 90, lane L5):
    // the `init-*` rows time the initialization a define asked for
    // (`define_class_full`'s `initialize`, armed there); `false` and no clock
    // otherwise, and for every initialization nested inside this one.
    use cratonvm_classloading::define_census::{
        arm_define_link, phase_end, phase_guard, phase_start_if, take_define_init, DefinePhase,
    };
    let census_init = take_define_init();
    let _census_body = phase_guard(DefinePhase::InitBody, phase_start_if(census_init));

    // Step 0 (HotSpot `link_class_impl`, interpreter round i1 wave 17): link
    // the superclass and the superinterfaces before this class, so each is
    // verified, prepared and reported first, and a supertype that fails to
    // link stops this class before it is prepared: a link failure of this
    // class too (`failure_state` is still `LINK_FAILED_STATE`).
    let census_clock = phase_start_if(census_init);
    link_supertypes_on_thread(shared, thread, class_id)?;
    phase_end(DefinePhase::InitLinkSupertypes, census_clock);
    // Steps 1-2: this class's verification and preparation (skipped when a
    // `link_class_on_thread` already did them).
    let census_clock = phase_start_if(census_init);
    arm_define_link(census_init);
    link_claimed_class(shared, thread, class_id, Some(&failure_state))?;
    phase_end(DefinePhase::InitLinkSelf, census_clock);
    let census_clock = phase_start_if(census_init);

    // Step 3: Initialize the superclass.
    let superclass_id = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .and_then(|c| c.superclass);
    if let Some(super_id) = superclass_id {
        if let Err(e) = ensure_class_initialized_shared(shared, thread, super_id) {
            if supertype_failed_to_link(shared, super_id) {
                failure_state.set(LINK_FAILED_STATE);
            } else {
                // JVMS §5.5 step 7: this class becomes erroneous too, with the
                // superclass's exception as its recorded cause.
                crate::runtime::exceptions::record_class_init_error(shared, thread, class_id, &e);
            }
            return Err(e);
        }
    }

    // Step 3.5: Initialize the superinterfaces that declare a non-abstract,
    // non-static (default) method, per JVMS §5.5 step 7, in its order and for
    // a class only (`superinterfaces_to_initialize`). The criterion is
    // *default methods*, NOT static fields: a superinterface that only
    // declares a static field is initialized lazily at the `getstatic` that
    // reads the field, never eagerly here. (See `interface_has_default_method`
    // doc — the old static-field criterion broke kotlin-reflect circular init.)
    //
    // An interface another thread is initializing is NOT skipped: JVMS §5.5
    // step 2 blocks until that initialization completes, as the superclass
    // step does (`ensure_class_initialized_shared` returns at once for a
    // recursive request on this thread). Until wave 10 an `Initializing`
    // interface was filtered out here whichever thread held it, so this class
    // could finish initializing before its superinterface had.
    {
        let iface_ids: Vec<ClassId> =
            superinterfaces_to_initialize(&shared.classes.class_manager.read(), class_id);
        for iface_id in iface_ids {
            let not_inited = shared
                .classes
                .class_manager
                .read()
                .get_class(iface_id)
                .map(|iface| iface.state != ClassState::Initialized)
                .unwrap_or(false);
            if not_inited {
                if let Err(e) = ensure_class_initialized_shared(shared, thread, iface_id) {
                    if supertype_failed_to_link(shared, iface_id) {
                        failure_state.set(LINK_FAILED_STATE);
                    } else {
                        crate::runtime::exceptions::record_class_init_error(
                            shared, thread, class_id, &e,
                        );
                    }
                    return Err(e);
                }
            }
        }
    }

    phase_end(DefinePhase::InitSupertypes, census_clock);
    let _census_clinit = phase_guard(DefinePhase::InitClinit, phase_start_if(census_init));

    // Step 4: Execute <clinit> (Prepared -> Initializing -> Initialized)
    // Note: the Initializing state and waiter condvar are already set by the
    // caller (ensure_class_initialized_shared) to avoid TOCTOU races.
    // We still need to set the state here for the verification/preparation
    // steps above that may have changed it to Verified/Prepared.
    let current_thread_id = thread.thread_id.0;
    {
        let mut cm = shared.classes.class_manager_write();
        if let Some(class) = cm.get_class_mut(class_id) {
            // A claim lost to a stub upgrade (`--compatible`) linked the
            // stub, not the real class: never re-assert it over the real
            // class (that would skip the real class's link, and override the
            // claim of a thread that took it since). `Ok` without finalizing:
            // the guard releases the waiters and the caller retries
            // (wave 62, lane L5).
            if !init_claim_held(class, current_thread_id) {
                return Ok(());
            }
            class.state = ClassState::Initializing;
            // JIT round 21 wave 1 (lane indy, INDY21-2; review rev-vmjit item 2):
            // a compiled class-init barrier's owner cell names the claimant
            // from HERE -- the INIT claim's `Initializing` transition, under
            // this write lock -- never from a link-only claim, and never for
            // a `--compatible` stub (whose in-place upgrade clears the claim
            // with no hook; a real class is never upgraded).
            if !class.origin.is_compatibility_stub() {
                crate::runtime::interpreter::jit_bridge::clinit_owner_note(
                    shared,
                    class_id,
                    Some(current_thread_id),
                );
            }
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
            let (out_ref, err_ref) = shared.try_ensure_system_streams()
                .map_err(|e| MethodCallFailed::InternalError(VmError::Runtime(e)))?;
            // gen r4w3/rooting: `out_ref` / `err_ref` are stored into the
            // System statics below, after the stdin setup (which can run Java
            // and move them). Pin both and re-read them afterwards.
            let streams_pin_base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(out_ref);
            thread.native_pin_roots.push(err_ref);
            let in_ref = match ensure_system_stdin_object(shared, thread) {
                Ok(r) => r,
                Err(e) => {
                    thread.native_pin_roots.truncate(streams_pin_base);
                    return Err(e);
                }
            };
            // `System.in` is a `BufferedInputStream` on every real JVM, and the
            // difference is reachable: `System.in.markSupported()` answers
            // false without it, so a parser that asks before looking ahead
            // takes the wrong branch. WORKER-4-NOTE-4.
            //
            // Pinned across `wrap_stdin_buffered` (which allocates) so the
            // fallback below cannot read a stale address if wrapping fails.
            let in_pin_base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(in_ref);
            let in_static = wrap_stdin_buffered(shared, thread, in_ref);
            let in_ref = thread.native_pin_roots[in_pin_base];
            thread.native_pin_roots.truncate(in_pin_base);
            let in_static = in_static.unwrap_or(in_ref);
            let out_ref = thread.native_pin_roots[streams_pin_base];
            let err_ref = thread.native_pin_roots[streams_pin_base + 1];
            thread.native_pin_roots.truncate(streams_pin_base);
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
                super::set_static_shared(shared, class_id, in_idx, Value::Object(Some(in_static)));
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
    // A claim lost to a stub upgrade publishes nothing (`finalize_class_init`)
    // and the caller retries, so the real `<clinit>` runs.
    let finalize_init = |shared: &SharedVm, class_id: ClassId, new_state: ClassState| {
        finalize_class_init(shared, class_id, new_state, current_thread_id);
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
                jfr_event_class_initialized(
                    shared,
                    thread,
                    class_id,
                    init_start.elapsed().as_nanos() as u64,
                );
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
                    eprintln!("MODSTATIC: Module <clinit> FAILED: {:?}", e);
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
                        &format!("class={} err={:?}", class_name_for_jfr, e),
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
                        class_name_for_jfr, e
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
                            if let Some(frames) = shared.throwable_stack_trace_for(*exc_ref) {
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
                // Before the state flips, so a waiter woken by it already
                // sees the recorded cause (HotSpot's step 10/11 order).
                crate::runtime::exceptions::record_class_init_error(shared, thread, class_id, &e);
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
                            //
                            // At `info`, not `warn` (interpreter round i2 wave
                            // 60, lane L5): HotSpot prints nothing for a
                            // `<clinit>` that throws until the error is
                            // reported, and a program that catches it prints
                            // nothing at all (probe
                            // `L5W59LauncherClinitThrows`). The launcher's
                            // default floor is `warn`, so every caught
                            // optional-dependency probe put these lines on
                            // stderr. `RUST_LOG=info` (kept in release by
                            // `release_max_level_info`) shows them again; the
                            // block is skipped when that level is off.
                            if tracing::enabled!(tracing::Level::INFO) {
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
                                tracing::info!(
                                    class = %class_name_for_jfr,
                                    cause = %cause_class,
                                    message = %cause_msg,
                                    "<clinit> failed вЂ” wrapping in ExceptionInInitializerError"
                                );
                                // Diagnostic: dump captured stack trace from
                                // the VM-wide Throwable trace registry so we can pinpoint where
                                // bare-NPEs originate during boot.
                                if let Some(frames) = shared.throwable_stack_trace_for(*exc_ref) {
                                    for (i, f) in frames.iter().enumerate().take(20) {
                                        tracing::info!(
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
                                    tracing::info!("  [CLINIT-TRACE] no captured frames for obj={:?} — falling back to live thread.frames", exc_ref.as_ptr());
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
                                        tracing::info!(
                                            "  [CLINIT-LIVE {}] at {}.{} pc={}",
                                            i,
                                            cn,
                                            f.method_name(),
                                            f.pc,
                                        );
                                    }
                                }
                                // FRAME-VECTOR-ON-CCE diagnostic (see
                                // docs/internal/jdk-only/
                                // junit-launcher-init-classcastexception-arrayspliterator-rare-flake-RETIRED-20260921.md,
                                // "What would settle it" §2): the one real
                                // occurrence on record printed a captured
                                // `[CLINIT-TRACE]` whose frames could not have
                                // come from one call chain (frames from
                                // unrelated methods interleaved), which reads
                                // as corruption of the trace-capture path OR
                                // of the live frame state it was built from.
                                // Unlike the `CLINIT-LIVE` fallback above
                                // (which only fires when NO captured trace
                                // exists), this dumps the LIVE interpreter
                                // frame vector *in addition to* a captured
                                // trace whenever the wrapped exception is a
                                // `ClassCastException` — cheap (this branch is
                                // already off the hot path; a `<clinit>`
                                // failure of any kind is rare) and it is the
                                // one comparison that can tell a future
                                // occurrence apart from this one: if the live
                                // frames are internally consistent while the
                                // captured trace is scrambled, the corruption
                                // is in trace capture, not in the frames
                                // themselves; if the live frames are ALSO
                                // inconsistent (wrong class/method/pc
                                // combinations, or a bogus `class_id`), the
                                // corruption already existed in the running
                                // thread's own frame stack.
                                if &*cause_class == "java/lang/ClassCastException" {
                                    let cm = shared.classes.class_manager.read();
                                    tracing::info!(
                                        "  [CLINIT-CCE-FRAMEVEC] {} live frame(s) at the moment \
                                         this ClassCastException was wrapped (deepest first):",
                                        thread.frames.len()
                                    );
                                    for (i, f) in thread.frames.iter().enumerate().rev().take(40) {
                                        let cn = cm
                                            .get_class(f.class_id)
                                            .map(|c| c.name.to_string())
                                            .unwrap_or_else(|| {
                                                format!("<unresolved class_id={:?}>", f.class_id)
                                            });
                                        tracing::info!(
                                            "  [CLINIT-CCE-FRAMEVEC {}] class_id={:?} {}.{} pc={} \
                                             locals={} operand_stack_depth={}",
                                            i,
                                            f.class_id,
                                            cn,
                                            f.method_name(),
                                            f.pc,
                                            f.locals_len(),
                                            f.stack.len(),
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
                                        tracing::info!(
                                            "  [CLINIT-CAUSE depth={}] {}: {}",
                                            depth,
                                            cause_name,
                                            cause_msg,
                                        );
                                        if let Some(frames) =
                                            shared.throwable_stack_trace_for(cause_obj)
                                        {
                                            for (i, f) in frames.iter().enumerate().take(15) {
                                                tracing::info!(
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
                            // gen r4w3/rooting: constructing the EIIE runs its
                            // `<init>` and `initCause` below runs Java (GC
                            // points); `cause_ref` and then `eiie_ref` are
                            // pinned and re-read after each.
                            let cause_pin = thread.native_pin_roots.len();
                            thread.native_pin_roots.push(cause_ref);
                            let created = crate::runtime::exceptions::create_exception_object(
                                shared,
                                thread,
                                "java/lang/ExceptionInInitializerError",
                                None,
                            );
                            let cause_ref = thread.native_pin_roots[cause_pin];
                            match created {
                                Ok(eiie_ref) => {
                                    thread.native_pin_roots.push(eiie_ref);
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
                                        // HotSpot's `new ExceptionInInitializerError(thrown)`
                                        // leaves exactly this: `cause == thrown`.
                                        shared.mem.heap.set_field(
                                            eiie_ref,
                                            i,
                                            Value::Object(Some(cause_ref)),
                                        );
                                    } else {
                                        // No `cause` field to write (a layout
                                        // without one): ask `initCause`.
                                        //
                                        // Only here (interpreter round i1 wave 9,
                                        // L5). After the direct write above the
                                        // call can only fail — the no-arg
                                        // `ExceptionInInitializerError()` already
                                        // ran `initCause(null)`, so `cause != this`
                                        // — and failing builds an
                                        // `IllegalStateException` whose message
                                        // embeds `cause.toString()`: a
                                        // stack-traced allocation per failed
                                        // `<clinit>`, and a call into the
                                        // application's `toString()` that HotSpot
                                        // never makes.
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
                                    }
                                    let eiie_ref = thread.native_pin_roots[cause_pin + 1];
                                    thread.native_pin_roots.truncate(cause_pin);
                                    Err(MethodCallFailed::ExceptionThrown(eiie_ref))
                                }
                                Err(_) => {
                                    // gen r4w3/rooting: same as propagating `e`
                                    // (ExceptionThrown of the cause), but with
                                    // the cause's current address.
                                    thread.native_pin_roots.truncate(cause_pin);
                                    Err(MethodCallFailed::ExceptionThrown(cause_ref))
                                }
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

        jfr_event_class_initialized(
            shared,
            thread,
            class_id,
            init_start.elapsed().as_nanos() as u64,
        );
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
/// What `AbstractValueLayout.carrier` should hold for a preseeded layout:
/// either a primitive type name (resolved through the same primitive-mirror
/// cache `Integer.TYPE` etc. use) or a reference class to mint a `Class`
/// mirror for (`MemorySegment`, for the two `ADDRESS` layouts).
#[derive(Clone, Copy)]
enum PreseedCarrier {
    Primitive(&'static str),
    Reference(&'static str),
}

/// The CONCRETE carrier for each preseeded `ValueLayout` static, with its size
/// and alignment and the `AbstractValueLayout.carrier` it must report.
///
/// These were the INTERFACE names until 2026-08-29, which is what made the
/// preseed a fabrication rather than a layout: an interface declares no
/// instance fields, so the two slots it wrote were invented. HotSpot answers
/// `jdk.internal.foreign.layout.ValueLayouts$OfIntImpl` for
/// `ValueLayout.JAVA_INT.getClass()`, and so does the strict arm, which drops
/// this preseed entirely.
fn value_layout_preseed(field_name: &str) -> Option<(&'static str, i64, i64, PreseedCarrier)> {
    const OF_BYTE: &str = "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl";
    const OF_BOOLEAN: &str = "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl";
    const OF_CHAR: &str = "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl";
    const OF_SHORT: &str = "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl";
    const OF_INT: &str = "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl";
    const OF_LONG: &str = "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl";
    const OF_FLOAT: &str = "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl";
    const OF_DOUBLE: &str = "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl";
    const OF_ADDRESS: &str = "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl";
    // `ValueLayouts$OfAddressImpl`'s carrier is `MemorySegment.class`, not a
    // primitive -- see its constructor, which passes the literal
    // `MemorySegment.class` up to `AbstractValueLayout`.
    const ADDRESS_CARRIER: PreseedCarrier =
        PreseedCarrier::Reference("java/lang/foreign/MemorySegment");
    match field_name {
        "ADDRESS" => Some((OF_ADDRESS, 8, 8, ADDRESS_CARRIER)),
        "JAVA_BYTE" => Some((OF_BYTE, 1, 1, PreseedCarrier::Primitive("byte"))),
        "JAVA_BOOLEAN" => Some((OF_BOOLEAN, 1, 1, PreseedCarrier::Primitive("boolean"))),
        "JAVA_CHAR" => Some((OF_CHAR, 2, 2, PreseedCarrier::Primitive("char"))),
        "JAVA_SHORT" => Some((OF_SHORT, 2, 2, PreseedCarrier::Primitive("short"))),
        "JAVA_INT" => Some((OF_INT, 4, 4, PreseedCarrier::Primitive("int"))),
        "JAVA_LONG" => Some((OF_LONG, 8, 8, PreseedCarrier::Primitive("long"))),
        "JAVA_FLOAT" => Some((OF_FLOAT, 4, 4, PreseedCarrier::Primitive("float"))),
        "JAVA_DOUBLE" => Some((OF_DOUBLE, 8, 8, PreseedCarrier::Primitive("double"))),
        "ADDRESS_UNALIGNED" => Some((OF_ADDRESS, 8, 1, ADDRESS_CARRIER)),
        "JAVA_CHAR_UNALIGNED" => Some((OF_CHAR, 2, 1, PreseedCarrier::Primitive("char"))),
        "JAVA_SHORT_UNALIGNED" => Some((OF_SHORT, 2, 1, PreseedCarrier::Primitive("short"))),
        "JAVA_INT_UNALIGNED" => Some((OF_INT, 4, 1, PreseedCarrier::Primitive("int"))),
        "JAVA_LONG_UNALIGNED" => Some((OF_LONG, 8, 1, PreseedCarrier::Primitive("long"))),
        "JAVA_FLOAT_UNALIGNED" => Some((OF_FLOAT, 4, 1, PreseedCarrier::Primitive("float"))),
        "JAVA_DOUBLE_UNALIGNED" => Some((OF_DOUBLE, 8, 1, PreseedCarrier::Primitive("double"))),
        _ => None,
    }
}

/// A `java.nio.ByteOrder` value for the `order` field of a preseeded
/// `ValueLayout` static -- the host's native order, matching what every real
/// `ValueLayout.<clinit>` builds its constants from (`ByteOrder.nativeOrder()`).
///
/// Preseeding runs during class PREPARATION (JVMS §5.5 step 9, before
/// `<clinit>`), which has no `JvmThread` in hand and must not run bytecode --
/// so `java/nio/ByteOrder`'s own `<clinit>` cannot be forced here the way a
/// native call (`p67_canonical_byte_order`) can. When that class happens to
/// be `Initialized` already, its canonical `LITTLE_ENDIAN`/`BIG_ENDIAN`
/// singleton is read and returned, matching identity with
/// `ByteOrder.nativeOrder()`. Otherwise a same-shaped stand-in is minted with
/// its `name` field set, so real bytecode (`AbstractValueLayout.equals()`,
/// `toString()`) sees a well-typed, correctly-named object rather than an
/// unwritten null -- the same tradeoff `p67_byte_order_object`'s fallback
/// makes for the native-call path.
fn resolve_preseed_byte_order(shared: &SharedVm) -> Result<Option<Value>, PreseedOom> {
    resolve_preseed_byte_order_inner(shared)
}

/// zr z4/ab: a ValueLayout preseed allocation the heap refused. The preseed
/// runs inside `prepare_class` (an interface's statics, after start-up: the
/// first FFM use), which reports `OutOfMemoryError` on its own `Result`; the
/// preseeds used the infallible entry and aborted the process on a full ZGC
/// heap instead.
#[derive(Debug, Clone, Copy)]
pub(super) struct PreseedOom;

fn resolve_preseed_byte_order_inner(shared: &SharedVm) -> Result<Option<Value>, PreseedOom> {
    let Some(v) = resolve_preseed_byte_order_found(shared) else {
        return Ok(None);
    };
    match v {
        PreseedOrder::Value(v) => Ok(Some(v)),
        PreseedOrder::StandIn {
            class_id,
            num_fields,
            name_field_idx,
            order_name,
        } => {
            let obj = preseed_alloc_object(shared, class_id, num_fields)?;
            if let Some(name_idx) = name_field_idx {
                if let Some(name_ref) = super::vm_object::try_create_java_string(shared, order_name)
                {
                    shared
                        .mem
                        .heap
                        .set_field(obj, name_idx, Value::Object(Some(name_ref)));
                }
            }
            Ok(Some(Value::Object(Some(obj))))
        }
    }
}

/// Collection-free preseed allocation, reporting refusal on every collector.
fn preseed_alloc_object(
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, PreseedOom> {
    shared.mem.heap.try_alloc_object_full(class_id, num_fields).ok_or(PreseedOom)
}

enum PreseedOrder {
    Value(Value),
    StandIn {
        class_id: ClassId,
        num_fields: usize,
        name_field_idx: Option<usize>,
        order_name: &'static str,
    },
}

fn resolve_preseed_byte_order_found(shared: &SharedVm) -> Option<PreseedOrder> {
    let little_endian = cfg!(target_endian = "little");
    let order_name = if little_endian {
        "LITTLE_ENDIAN"
    } else {
        "BIG_ENDIAN"
    };
    let class_id = shared.load_class_concurrent("java/nio/ByteOrder").ok()?;
    let (is_initialized, num_fields, name_field_idx) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let name_field_idx =
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(class_id, "name", &cm.class_store);
        (
            class.state == ClassState::Initialized,
            class.num_total_fields.max(1),
            name_field_idx,
        )
    };
    if is_initialized {
        let mut static_idx = 0usize;
        let mut found = None;
        let cm = shared.classes.class_manager.read();
        if let Some(class) = cm.get_class(class_id) {
            for f in &class.fields {
                if f.is_static() {
                    if &*f.name == order_name {
                        found = Some(static_idx);
                        break;
                    }
                    static_idx += 1;
                }
            }
        }
        drop(cm);
        if let Some(static_idx) = found {
            if let Value::Object(Some(o)) =
                super::vm_object::get_static_shared(shared, class_id, static_idx)
            {
                return Some(PreseedOrder::Value(Value::Object(Some(o))));
            }
        }
    }
    // Fallback: a same-shaped stand-in, correctly named (allocated by the
    // caller, which can report a refusal).
    Some(PreseedOrder::StandIn {
        class_id,
        num_fields,
        name_field_idx,
        order_name,
    })
}

/// A same-shaped, non-canonical `java.util.Optional` with an unset `value`
/// field -- i.e. empty, the way `native-builtins`'s `p67_optional` mints one
/// for a layout built at runtime. `Optional.equals()` compares by value
/// (`Objects.equals(this.value, other.value)`), not identity, so a fresh
/// instance here is exactly as correct as the canonical `Optional.empty()`
/// singleton for every real-bytecode reader (`equals()`, `toString()`).
fn preseed_empty_optional(shared: &SharedVm) -> Result<Option<Value>, PreseedOom> {
    let Ok(class_id) = shared.load_class_concurrent("java/util/Optional") else {
        return Ok(None);
    };
    let num_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.num_total_fields.max(1))
        .unwrap_or(1);
    let obj = preseed_alloc_object(shared, class_id, num_fields)?;
    shared.mem.heap.set_field(obj, 0, Value::Object(None));
    Ok(Some(Value::Object(Some(obj))))
}

fn make_prepared_value_layout(
    shared: &SharedVm,
    class_name: &str,
    byte_size: i64,
    byte_alignment: i64,
    carrier: PreseedCarrier,
) -> Result<Option<ObjectRef>, PreseedOom> {
    let Ok(layout_class_id) = shared.load_class_concurrent(class_name) else {
        return Ok(None);
    };
    let num_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(layout_class_id)
        .map(|c| c.num_total_fields.max(2))
        .unwrap_or(2);
    let obj = preseed_alloc_object(shared, layout_class_id, num_fields)?;
    // BY NAME, now that the carrier is a class that declares the fields.
    // `AbstractLayout` gives every layout `byteSize`, `byteAlignment` and
    // `name`; `AbstractValueLayout` adds `carrier` and `order`. The positional
    // 0/1 below is the fallback for a build that cannot load the impl class,
    // which is the old behaviour exactly.
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
            (Some(size_slot), Some(align_slot)) => Some((
                size_slot,
                align_slot,
                idx("name"),
                idx("carrier"),
                idx("order"),
            )),
            _ => None,
        }
    };
    if let Some((size_slot, align_slot, name_slot, carrier_slot, order_slot)) = named {
        shared
            .mem
            .heap
            .set_field(obj, size_slot, Value::Long(byte_size));
        shared
            .mem
            .heap
            .set_field(obj, align_slot, Value::Long(byte_alignment));
        // A real `AbstractLayout.name` is `Optional<String>`, and real
        // bytecode (`AbstractLayout.equals()`, `toString()`) reads the field
        // and calls `.equals()`/methods on it directly -- a bare Java `null`
        // there NPEs out of `equals()` the moment two DIFFERENT preseeded
        // layout objects are compared (`JAVA_INT.equals(JAVA_INT
        // .withByteAlignment(4))`, measured). `p67_layout_set_name` makes the
        // same distinction for layouts built at runtime; mint the
        // same-shaped-but-not-canonical `Optional` stand-in that helper (and
        // `p67_optional`) already use -- empty (`value` unset) rather than a
        // raw null. `name()` itself is natively intercepted (`p67_layout_name`)
        // and already tolerates either shape identically.
        if let Some(name_slot) = name_slot {
            let empty_name = preseed_empty_optional(shared)?.unwrap_or(Value::Object(None));
            shared.mem.heap.set_field(obj, name_slot, empty_name);
        }
        // `carrier`: the missing field that made
        // `ValueLayout.JAVA_INT.withName("k").equals(...)` throw
        // `NullPointerException` -- `AbstractValueLayout.equals()` reads
        // `this.carrier` directly and calls `.equals()` on it, and a null
        // receiver there NPEs regardless of which side is unequal.
        if let Some(carrier_slot) = carrier_slot {
            let mirror = match carrier {
                PreseedCarrier::Primitive(prim) => Some(
                    super::vm_object::try_get_or_create_primitive_mirror(shared, prim)
                        .ok_or(PreseedOom)?,
                ),
                PreseedCarrier::Reference(cls) => match shared.load_class_concurrent(cls) {
                    Ok(cid) => Some(
                        super::vm_object::try_get_or_create_class_mirror(shared, cid)
                            .ok_or(PreseedOom)?,
                    ),
                    Err(_) => None,
                },
            };
            if let Some(mirror) = mirror {
                shared
                    .mem
                    .heap
                    .set_field(obj, carrier_slot, Value::Object(Some(mirror)));
            }
        }
        // `order`: see `resolve_preseed_byte_order` for why this cannot always
        // be the canonical `ByteOrder.nativeOrder()` singleton at this point
        // in class loading.
        if let Some(order_slot) = order_slot {
            if let Some(order_value) = resolve_preseed_byte_order(shared)? {
                shared.mem.heap.set_field(obj, order_slot, order_value);
            }
        }
        return Ok(Some(obj));
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
    Ok(Some(obj))
}

/// gcg g1b/o: may `link_claimed_class` collect and retry a failed
/// preparation? Only a heap refusal (`prepare_class_shared`'s
/// `ConstantValue` String), only on the initialization path, and not under
/// `CRATONVM_GC_CLASS_PREP_RECLAIM=0` (default ON; `runtime_flag_default_on`).
fn prep_refusal_may_reclaim(err: &VmError, initializing: bool) -> bool {
    initializing
        && matches!(err, VmError::Runtime(RuntimeError::OutOfMemoryError { .. }))
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_CLASS_PREP_RECLAIM")
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

    let (class_name, static_field_info, soft_clock_slot) = {
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
                        ConstantPoolEntry::Long(v) => Some(CvSeed::Primitive(Value::Long(v.get()))),
                        ConstantPoolEntry::Double(v) => {
                            Some(CvSeed::Primitive(Value::Double(v.get())))
                        }
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
        let soft_clock_slot = if class.loader_id == cratonvm_types::ClassLoaderId::Bootstrap
            && &*class.name == "java/lang/ref/SoftReference"
        {
            info.iter().position(|(name, descriptor, _)| name == "clock" && descriptor == "J")
        } else {
            None
        };
        (class.name.to_string(), info, soft_clock_slot)
    };

    // HotSpot's ReferenceProcessor::init_statics seeds this VM-owned field
    // before any Java constructor/get() can copy it. This is a default seed:
    // normal ConstantValue and earlier static stores retain their precedence.
    let soft_clock_seed = soft_clock_slot
        .map(|_| shared.mem.ref_processor.lock().soft_clock_for_java_initialization());

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
    // Same trade for `op_getstatic`'s synthetic-`Boolean` repair; see
    // `ClassRealm::boolean_class_id`.
    if class_name == "java/lang/Boolean" {
        shared
            .classes
            .boolean_class_id
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
        if soft_clock_slot == Some(static_idx) {
            let clock = soft_clock_seed.expect("clock slot has a seed");
            statics[static_idx] = Value::Long(clock as i64);
        }

        match seed {
            // A sub-int field's `ConstantValue` is an `Integer` entry of any
            // value (the format check asks only for the tag); HotSpot stores
            // it through `byte_field_put` / `char_field_put` /
            // `short_field_put` / `bool_field_put`, i.e. narrowed to the
            // field's width (`initialize_static_primitive_field`). Stored
            // whole, `static final byte B` with `ConstantValue 300` read 300
            // through `getstatic`, which does not re-narrow (interpreter
            // round i4 wave 98, lane L7; probe
            // `tools/probes/interp/L7/L7W98ConstantValueNarrowing.java`).
            Some(CvSeed::Primitive(v)) => {
                statics[static_idx] = match (*v, descriptor.as_bytes().first()) {
                    (Value::Int(x), Some(b'B')) => Value::Int(i32::from(x as i8)),
                    (Value::Int(x), Some(b'S')) => Value::Int(i32::from(x as i16)),
                    (Value::Int(x), Some(b'C')) => Value::Int(i32::from(x as u16)),
                    (Value::Int(x), Some(b'Z')) => Value::Int(x & 1),
                    _ => *v,
                }
            }
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
                // gci i39/rw (`gci-i39rw-lone-surrogate-constantvalue-string-is-not-the-literal-FIXED-20261016`):
                // the interned literal, as HotSpot's `string_initial_value`
                // (`StringTable::intern`); it was an unpooled String, so
                // `Field.get` answered neither the `ldc` literal nor
                // `intern()`'s object. `CRATONVM_GC_SURROGATE_CONSTANT_VALUE_INTERNED=0`
                // restores the unpooled String. Collection-free, like the
                // `StringUtf8` arm: no allocation in this loop can collect,
                // so the raw refs in `statics` stay valid until publication.
                let made = if crate::runtime::interpreter::constants::surrogate_constant_value_interned() {
                    crate::runtime::interpreter::constants::intern_wide_string_constant_value(
                        shared, units,
                    )
                } else {
                    super::vm_object::try_create_java_string_from_units(shared, units)
                };
                let str_ref = made
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
            if let Some((layout_class, byte_size, byte_alignment, carrier)) =
                value_layout_preseed(field_name)
            {
                let made = make_prepared_value_layout(
                    shared,
                    layout_class,
                    byte_size,
                    byte_alignment,
                    carrier,
                )
                .map_err(|PreseedOom| {
                    VmError::Runtime(RuntimeError::OutOfMemoryError {
                        message: format!(
                            "Java heap space (prepare_class {class_name}.{field_name} layout preseed)"
                        ),
                    })
                })?;
                if let Some(obj) = made {
                    statics[static_idx] = Value::Object(Some(obj));
                }
            }
        }
    }

    // Publish to the lock-free index BEFORE inserting into the map, so a
    // concurrent reader either sees nothing (and takes the locked path, which
    // blocks on the write below) or sees a fully-populated block. Both happen
    // under the `statics` write lock: `StaticsIndex::publish` requires its
    // callers to be serialized, and `set_static_shared` also publishes (a
    // grow) — two unserialized publications could leave one block's base
    // paired with the other's length.
    let mut statics_map = shared.classes.statics.write();
    // A static WRITTEN before preparation keeps its value (`set_static_shared`
    // makes the block then): HotSpot's statics live in the mirror from the
    // moment the class is loaded, so a store through
    // `Unsafe.putReference(staticFieldBase, staticFieldOffset, v)` on a loaded,
    // unlinked class is there after linking. Replacing the block wiped it:
    // `ClassSpecializer.Factory.linkCodeToSpeciesData` stores every salvaged
    // `BoundMethodHandle$Species_*`'s `BMH_SPECIES` that way, and its
    // `speciesData()` answered `null` (`NullPointerException ...
    // speciesData() is null` in every JDK `asType` / `bindTo` that extends a
    // species past `L`). Interpreter round i2 wave 56, lane L4 (page
    // `interpreter-L4-a-static-written-before-preparation-is-wiped-FIXED-20261020`).
    // A `ConstantValue` seed still wins: preparation assigns it.
    if statics_map.contains_key(&class_id) {
        for (static_idx, (_, _, seed)) in static_field_info.iter().enumerate() {
            if seed.is_none() {
                if let Some(written) = shared.classes.statics_index.get(class_id, static_idx) {
                    statics[static_idx] = written;
                } else if let Some(written) = statics_map
                    .get(&class_id)
                    .and_then(|old| old.as_slice().get(static_idx).copied())
                {
                    statics[static_idx] = written;
                }
            }
        }
    }
    let block = crate::vm::realms::class_realm::StaticsBlock::from_values(statics);
    shared.classes.statics_index.publish(class_id, &block);
    statics_map.insert(class_id, block);
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
        ConstantPoolEntry::Long(v) => Some(Value::Long(v.get())),
        ConstantPoolEntry::Double(v) => Some(Value::Double(v.get())),
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
                // gc-common w8-a: per VM, the same call `Unsafe.staticFieldOffset`
                // makes (a `ClassId` means nothing outside the VM that minted it).
                let offset = cratonvm_native_builtins::register_static_field_offset_in_vm(
                    shared.vm_identity,
                    "sun/misc/Unsafe",
                    "memoryAccessWarned",
                    class_id,
                    widx,
                );
                // The class mirror is what `native_unsafe_static_field_base`
                // answers for a static field on this VM, and `StaticBaseProbe`
                // proved that shape round-trips byte-identically to HotSpot.
                // zr z4/ab: the fallible mirror. `sun.misc.Unsafe` initialises
                // on its first use, after start-up; on a heap that cannot hold
                // the mirror the latch is left unset (a later use re-derives
                // it), where the infallible mirror aborted the process on ZGC.
                let mirror = super::vm_object::try_get_or_create_class_mirror(shared, class_id);
                let mut latch = 0;
                if let Some(mirror) = mirror {
                    latch += set_static_if_zero("MEMORY_ACCESS_WARNED_OFFSET", Value::Long(offset))
                        as i32;
                    latch += set_static_if_zero(
                        "MEMORY_ACCESS_WARNED_BASE",
                        Value::Object(Some(mirror)),
                    ) as i32;
                }
                note_fixup_outcome(
                    "sun.misc.Unsafe memory-access latch",
                    i64::from(latch),
                    i64::from(already_populated.get() - already_arrays),
                    2,
                );
            } else {
                tracing::warn!(
                    "Post-clinit fixup: sun.misc.Unsafe memoryAccessWarned not found — \
                     latch NOT repaired"
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
            // gcg g4rs (GC wave 9, lane gcD): the two pooled separators are made
            // here, just before they are stored, not before the class load above.
            // Under the weak String table (default on Generational) a pooled
            // String only a Rust local holds is not a root, so a pause inside
            // `load_class_concurrent` could free it.
            let sep_str = super::vm_object::create_java_string(shared, &sep_char.to_string());
            let path_sep_str =
                super::vm_object::create_java_string(shared, &path_sep_char.to_string());
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
                    // zr z4/ab: fallible like the object above (it was the one
                    // infallible allocation of this arm): on a refused array the
                    // JDK's own constant stays as `<clinit>` left it, and a
                    // fresh object is not published.
                    let mag_arr = shared.mem.heap.try_alloc_array(
                        ClassId::new(0), ArrayElementType::Int, mag_words.len(),
                    );
                    let Some(mag_arr) = mag_arr else {
                        return existing;
                    };
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

    /// Round 13 wave 10 (lane spin): `finalize_class_init` releases the
    /// `class_init_waiters` mutex BEFORE its `ClassManagerWriteGuard` drops.
    /// The guard's `Drop` re-takes `class_manager.read()` (deferred-`new`
    /// resweep, profile-replay seed), and a claimant in
    /// `ensure_class_initialized_shared` holds the write lock while it takes
    /// `class_init_waiters`; with the removal as the block's TAIL expression
    /// (whose temporary outlives `cm` in edition 2021) the two deadlocked --
    /// `R13CodecacheDiscardChurn`'s 1-in-15 `--compatible` hang. A source pin,
    /// because the behavioural reproduction needs a held deferred-`new` retry
    /// and loses the race most of the time.
    #[test]
    fn finalize_class_init_releases_the_waiter_mutex_before_the_write_guard_drops() {
        let src = include_str!("vm_util.rs");
        let start = src
            .find("\nfn finalize_class_init(")
            .unwrap_or_else(|| panic!("finalize_class_init not found"));
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
        // A `let` statement (wave 62 made it conditional: a claim lost to a
        // stub upgrade leaves a later claim's entry in place); its
        // temporaries end at its `;`.
        let removal = body
            .find("let removed = if release_waiter {\n            shared.classes.class_init_waiters.lock().remove(&class_id)\n        } else {\n            None\n        };")
            .unwrap_or_else(|| panic!("the waiter removal must be a `let` statement"));
        let drop_cm = body
            .find("drop(cm);")
            .unwrap_or_else(|| panic!("the write guard must be dropped explicitly"));
        assert!(
            removal < drop_cm,
            "the waiter mutex must be released before the class-manager write guard drops"
        );
    }

    /// The real bytes the stub-upgrade tests define over a stub named `Foo`.
    const UPGRADE_FOO: &[u8] =
        include_bytes!("../../../classloading/tests/fixtures/wp2_4b_redefine/Foo.v1.class");

    /// A stub `Foo` claimed for initialization by `claimant`, the way
    /// `ensure_class_initialized_shared` claims it, then upgraded in place to
    /// real bytes (`--compatible`): the race of
    /// `interpreter-L5-a-stub-upgrade-during-the-stubs-own-initialization-is-lost-FIXED-20261026.md`,
    /// interleaved by hand. Returns the class and the claim's waiter.
    fn claimed_stub_upgraded_in_place(
        shared: &SharedVm,
        claimant: u64,
    ) -> (
        ClassId,
        Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
    ) {
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("Foo", 0)
            .expect("Compatible mode fabricates");
        {
            let mut cm = shared.classes.class_manager_write();
            let class = cm.get_class_mut(id).expect("just fabricated");
            class.initializing_thread = Some(claimant);
            class.state = ClassState::Initializing;
            register_init_waiter(shared, id);
        }
        let waiter = shared
            .classes
            .class_init_waiters
            .lock()
            .get(&id)
            .cloned()
            .expect("the claim registered its waiter");
        let upgraded = shared
            .classes
            .class_manager
            .write()
            .define_class(
                "Foo",
                UPGRADE_FOO,
                cratonvm_types::ClassLoaderId::UserDefined(62),
            )
            .expect("real bytes upgrade the stub");
        assert_eq!(upgraded, id, "the upgrade keeps the stub's ClassId");
        (id, waiter)
    }

    /// Interpreter round i2 wave 62, lane L5: a claim that initialized the
    /// stub and finalizes after the upgrade must not mark the REAL class
    /// initialized (its `<clinit>` never ran). Before the fix the finalize
    /// wrote `Initialized`, `CLASS_INIT_INITIALIZED` and the bitmap bit, the
    /// predicted failure of the first assertion (positive control: the same
    /// finalize with the claim held publishes, see
    /// `a_held_claim_still_publishes_initialized`).
    #[test]
    fn a_claim_lost_to_a_stub_upgrade_does_not_publish_initialized() {
        let shared = test_shared();
        let (id, waiter) = claimed_stub_upgraded_in_place(&shared, 7);
        assert!(
            !finalize_class_init(&shared, id, ClassState::Initialized, 7),
            "the claim was lost to the upgrade"
        );
        {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(id).expect("still registered");
            assert_eq!(
                class.state,
                ClassState::Loaded,
                "left as the upgrade set it"
            );
            assert_eq!(class.initializing_thread, None);
            assert!(!is_class_initialized_fast(class));
        }
        assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
        assert!(!is_class_initialized_via_manager(&shared, id));
        assert!(
            *waiter.0.lock(),
            "nobody holds the class: the lost claim releases its waiters"
        );
        assert!(!shared.classes.class_init_waiters.lock().contains_key(&id));
        assert!(
            init_claim_was_lost(&shared, id),
            "the caller retries, for the real <clinit>"
        );
    }

    /// The positive control of the test above: the same claim, finalized
    /// while it still holds the class, publishes `Initialized`.
    #[test]
    fn a_held_claim_still_publishes_initialized() {
        let shared = test_shared();
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/L5W62HeldClaim", 0)
            .expect("Compatible mode fabricates");
        {
            let mut cm = shared.classes.class_manager_write();
            let class = cm.get_class_mut(id).expect("just fabricated");
            class.initializing_thread = Some(7);
            class.state = ClassState::Initializing;
            register_init_waiter(&shared, id);
        }
        assert!(finalize_class_init(&shared, id, ClassState::Initialized, 7));
        assert!(is_class_initialized_via_manager(&shared, id));
        assert!(!init_claim_was_lost(&shared, id));
    }

    /// A lost claim whose class another thread has claimed since leaves that
    /// claim alone, and its waiters with it: the later claim took over the
    /// waiter entry (`register_init_waiter`) and releases them at its own
    /// finalize.
    #[test]
    fn a_claim_lost_to_a_stub_upgrade_leaves_a_later_claim_alone() {
        let shared = test_shared();
        let (id, waiter) = claimed_stub_upgraded_in_place(&shared, 7);
        {
            let mut cm = shared.classes.class_manager_write();
            let class = cm.get_class_mut(id).expect("still registered");
            class.initializing_thread = Some(8);
            register_init_waiter(&shared, id);
        }
        assert!(!finalize_class_init(
            &shared,
            id,
            ClassState::Initialized,
            7
        ));
        {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(id).expect("still registered");
            assert_eq!(class.initializing_thread, Some(8), "the later claim stands");
            assert_eq!(class.state, ClassState::Loaded);
        }
        assert!(
            !*waiter.0.lock(),
            "not released before the later claim ends"
        );
        let shared_entry = shared
            .classes
            .class_init_waiters
            .lock()
            .get(&id)
            .cloned()
            .expect("the later claim's entry stays");
        assert!(
            Arc::ptr_eq(&shared_entry, &waiter),
            "the later claim took over the lost claim's waiter entry"
        );
        assert!(finalize_class_init(&shared, id, ClassState::Initialized, 8));
        assert!(*waiter.0.lock(), "the later claim's finalize releases them");
    }

    /// A VM with `CRATONVM_CLASS_INIT_BIT_FIRST` set to `on` (the field,
    /// whatever the environment says).
    fn shared_with_bit_first(on: bool) -> Arc<SharedVm> {
        let mut config = VmConfig::default();
        config.class_init_bit_first = on;
        Arc::new(SharedVm::new(config))
    }

    /// Interpreter round i3 wave 71, lane L3 (`VmConfig::class_init_bit_first`):
    /// with the switch on the bitmap is asked FIRST. A bit set on a class whose
    /// `init_state` was never marked (only the bitmap says "initialized") is
    /// answered `true` with the switch on and `false` with it off (the memo
    /// and the locked read see an uninitialized class): the positive control
    /// that the answer came from the bit.
    #[test]
    fn bit_first_answers_from_the_initialized_bitmap() {
        for on in [false, true] {
            let shared = shared_with_bit_first(on);
            let id = shared
                .classes
                .class_manager
                .write()
                .try_ensure_synthetic_class("cratonvm/test/L3W71BitOnly", 0)
                .expect("Compatible mode fabricates");
            let generation = shared.classes.class_init_bits.reset_generation();
            shared
                .classes
                .class_init_bits
                .set_as_of(id.as_u32(), generation);
            assert_eq!(
                is_class_initialized_via_manager(&shared, id),
                on,
                "bit_first={on}: only the switch makes the bit the answer"
            );
        }
    }

    /// With the switch on, the locked read that finds a class INITIALIZED
    /// which no `finalize_class_init` marked fills its bit; with it off the
    /// bitmap is left as it was (today's behaviour).
    #[test]
    fn bit_first_fills_the_bit_from_the_locked_read() {
        for on in [false, true] {
            let shared = shared_with_bit_first(on);
            let id = shared
                .classes
                .class_manager
                .write()
                .try_ensure_synthetic_class("cratonvm/test/L3W71Fill", 0)
                .expect("Compatible mode fabricates");
            shared
                .classes
                .class_manager
                .read()
                .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
            assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
            assert!(is_class_initialized_via_manager(&shared, id));
            assert_eq!(shared.classes.class_init_bits.is_set(id.as_u32()), on);
        }
    }

    /// The premise the switch rests on: a stub upgraded in place clears the
    /// bit the first read filled, so the bit-first answer is `false` after
    /// the upgrade (the real `<clinit>` has not run). Positive control: the
    /// same class answers `true` from the bit before the upgrade.
    #[test]
    fn bit_first_forgets_a_stub_upgraded_in_place() {
        let shared = shared_with_bit_first(true);
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("Foo", 0)
            .expect("Compatible mode fabricates");
        shared
            .classes
            .class_manager
            .read()
            .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
        assert!(is_class_initialized_via_manager(&shared, id));
        assert!(
            shared.classes.class_init_bits.is_set(id.as_u32()),
            "the read filled the bit"
        );
        let upgraded = shared
            .classes
            .class_manager
            .write()
            .define_class(
                "Foo",
                UPGRADE_FOO,
                cratonvm_types::ClassLoaderId::UserDefined(71),
            )
            .expect("real bytes upgrade the stub");
        assert_eq!(upgraded, id, "the upgrade keeps the stub's ClassId");
        assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
        assert!(
            !is_class_initialized_via_manager(&shared, id),
            "the upgraded class has not run its <clinit>"
        );
    }

    /// `finalize_class_init` sets the bit itself, so a class initialized the
    /// normal way answers from the bit at once with the switch on.
    #[test]
    fn bit_first_answers_a_finalized_class_from_its_bit() {
        let shared = shared_with_bit_first(true);
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class("cratonvm/test/L3W71Finalized", 0)
            .expect("Compatible mode fabricates");
        {
            let mut cm = shared.classes.class_manager_write();
            let class = cm.get_class_mut(id).expect("just fabricated");
            class.initializing_thread = Some(71);
            class.state = ClassState::Initializing;
            register_init_waiter(&shared, id);
        }
        assert!(finalize_class_init(
            &shared,
            id,
            ClassState::Initialized,
            71
        ));
        assert!(shared.classes.class_init_bits.is_set(id.as_u32()));
        assert!(is_class_initialized_via_manager(&shared, id));
    }

    /// Interpreter round i3 wave 72, lane L3
    /// (`i71-L3-proposal-retire-the-per-thread-class-init-memo`): with the
    /// bitmap on, a covered id is answered without the per-thread memo --
    /// the locked read fills the bit and leaves the memo empty; with the
    /// switch off (`=0`) the memo still serves (the read fills it, and the
    /// bitmap is left alone).
    #[test]
    fn the_memo_serves_only_with_the_bitmap_off() {
        for on in [false, true] {
            let shared = shared_with_bit_first(on);
            let id = shared
                .classes
                .class_manager
                .write()
                .try_ensure_synthetic_class("cratonvm/test/L3W72MemoOnlyOff", 0)
                .expect("Compatible mode fabricates");
            let raw = id.as_u32();
            assert!(shared.classes.class_init_bits.covers(raw));
            shared
                .classes
                .class_manager
                .read()
                .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
            let generation = shared.classes.class_init_bits.reset_generation();
            assert!(is_class_initialized_via_manager(&shared, id));
            assert_eq!(
                class_init_memo_hit(shared.vm_identity, raw, generation),
                !on,
                "bit_first={on}: only the switch-off path fills the memo"
            );
            assert_eq!(shared.classes.class_init_bits.is_set(raw), on);
        }
    }

    /// The bitmap's capacity bounds what it covers: ids past it keep the
    /// memo with the switch on (`ClassInitBits::covers`).
    #[test]
    fn the_bitmap_covers_ids_below_its_capacity_only() {
        let bits = cratonvm_classloading::ClassInitBits::new();
        assert!(bits.covers(0));
        assert!(bits.covers((1 << 20) - 1));
        assert!(!bits.covers(1 << 20));
        assert!(!bits.covers(u32::MAX));
    }

    /// A claim lost to an upgrade while it links does not move the real
    /// class's state: `link_claimed_class` writes nothing, so the class
    /// stays `Loaded` for the next claim to link from scratch.
    #[test]
    fn a_claim_lost_to_a_stub_upgrade_does_not_link_the_real_class() {
        let shared = test_shared();
        let (id, _waiter) = claimed_stub_upgraded_in_place(&shared, 7);
        let mut thread = JvmThread::new(ThreadId(7), "lost-claim");
        link_claimed_class(&shared, &mut thread, id, None).expect("a lost claim is not an error");
        assert_eq!(
            shared
                .classes
                .class_manager
                .read()
                .get_class(id)
                .map(|c| c.state),
            Some(ClassState::Loaded)
        );
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
            ConstantPoolEntry::Long(9999999.into()),
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
            ConstantPoolEntry::Double(3.25.into()),
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
    fn soft_reference_clock_preparation_respects_loader_and_existing_writes() {
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
        use cratonvm_reader::field::ClassFileField;
        use cratonvm_types::ClassLoaderId;
        for (name, loader, descriptor, seeded) in [
            ("java/lang/ref/SoftReference", ClassLoaderId::Bootstrap, "J", true),
            ("java/lang/ref/SoftReference", ClassLoaderId::Application, "J", false),
            ("OtherClock", ClassLoaderId::Bootstrap, "J", false),
            ("java/lang/ref/SoftReference", ClassLoaderId::Bootstrap, "I", false),
        ] {
            let shared = test_shared();
            let clock = shared
                .mem
                .ref_processor
                .lock()
                .soft_clock_for_java_initialization();
            assert!(clock > 0);
            let id = {
                let mut cm = shared.classes.class_manager_write();
                let mut class = make_malformed_class(name, loader);
                class.id = cm.class_store.next_id();
                class.state = ClassState::Verified;
                class.access_flags = ClassAccessFlags::PUBLIC;
                class.fields = vec![ClassFileField {
                    access_flags: FieldAccessFlags::STATIC,
                    name: Arc::from("clock"),
                    descriptor: Arc::from(descriptor),
                    attributes: Vec::new(),
                }];
                let id = class.id;
                cm.class_store.add(class);
                id
            };
            prepare_class_shared(&shared, id).unwrap();
            let expected = if seeded {
                Value::Long(clock as i64)
            } else {
                default_value_for_descriptor(descriptor)
            };
            assert_eq!(super::super::get_static_shared(&shared, id, 0), expected);
            if seeded {
                for earlier_write in [7, 0] {
                    super::super::set_static_shared(&shared, id, 0, Value::Long(earlier_write));
                    prepare_class_shared(&shared, id).unwrap();
                    assert_eq!(
                        super::super::get_static_shared(&shared, id, 0),
                        Value::Long(earlier_write)
                    );
                }
            }
        }
    }

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
            ConstantPoolEntry::Long((0xDEAD_BEEF_CAFE_BABEu64 as i64).into()),
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Double(3.141.into()),
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

    /// A static written before its class is prepared (`set_static_shared`
    /// makes the block then) survives preparation, as it survives linking on
    /// HotSpot; a `ConstantValue` seed is still assigned. Interpreter round i2
    /// wave 56, lane L4 (`ClassSpecializer`'s `BMH_SPECIES` store).
    #[test]
    fn a_static_written_before_preparation_survives_it() {
        use crate::classloading::{Class, ClassLoaderId, ClassState};
        use cratonvm_reader::attribute::Attribute;
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
        use cratonvm_reader::class_file_version::ClassFileVersion;
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use cratonvm_reader::field::ClassFileField;

        let cp = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Integer(42),
        ]);
        let fields = vec![
            ClassFileField {
                access_flags: FieldAccessFlags::STATIC,
                name: Arc::from("WRITTEN"),
                descriptor: Arc::from("J"),
                attributes: Vec::new(),
            },
            ClassFileField {
                access_flags: FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: Arc::from("SEEDED"),
                descriptor: Arc::from("I"),
                attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                    Attribute::ConstantValue {
                        constant_value_index: 1,
                    },
                )],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::STATIC,
                name: Arc::from("UNTOUCHED"),
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
                name: Arc::from("TestEarlyStatic"),
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

        super::super::vm_object::set_static_shared(&shared, class_id, 0, Value::Long(7));
        prepare_class_shared(&shared, class_id).expect("prepare_class_shared");

        let statics = shared.classes.statics.read();
        let slots = statics.get(&class_id).expect("statics for class");
        assert_eq!(slots[0], Value::Long(7), "the early write survives");
        assert_eq!(slots[1], Value::Int(42), "the ConstantValue is assigned");
        assert_eq!(
            slots[2],
            Value::Long(0),
            "an untouched static keeps its default"
        );
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

        // (1) A LINK failure parks the class unlinked (HotSpot leaves an
        // unlinkable class unlinked, not erroneous), and (2) the init claim is
        // cleared.
        {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(class_id).expect("class still registered");
            assert_eq!(
                class.state, LINK_FAILED_STATE,
                "a link failure must park the class unlinked, not InitializationError"
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

        // (4) A subsequent init attempt links again and fails with the SAME
        // linkage error (not "Could not initialize class"), and cleans up again.
        let again = ensure_class_initialized_shared(&shared, &mut thread, class_id);
        match again {
            Err(MethodCallFailed::InternalError(VmError::Linkage(_))) => {}
            other => {
                panic!("re-init after a link failure must relink and rethrow; got {other:?}")
            }
        }
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id).expect("class still registered");
        assert!(class_link_failed(class));
    }

    #[test]
    fn a_superclass_link_failure_leaves_the_subclass_retryable() {
        // HotSpot links a class's superclass as part of linking the class, so a
        // superclass that fails to link is a link failure of the subclass too:
        // both stay unlinked and a second use rethrows the linkage error.
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let (super_id, sub_id) = {
            let mut cm = shared.classes.class_manager_write();
            let super_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: super_id,
                // ANNOTATION without INTERFACE fails Pass 2 but is not FINAL,
                // so the subclass's own Pass 2 (`cannot extend final class`)
                // passes.
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::ANNOTATION,
                ..make_malformed_class("com/example/BadSuper", ClassLoaderId::Application)
            });
            let sub_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: sub_id,
                access_flags: ClassAccessFlags::PUBLIC,
                superclass: Some(super_id),
                ..make_malformed_class("com/example/GoodSub", ClassLoaderId::Application)
            });
            (super_id, sub_id)
        };
        for attempt in 0..2 {
            match ensure_class_initialized_shared(&shared, &mut thread, sub_id) {
                Err(MethodCallFailed::InternalError(VmError::Linkage(_))) => {}
                other => {
                    panic!(
                        "attempt {attempt}: expected the superclass's linkage error, got {other:?}"
                    )
                }
            }
            let cm = shared.classes.class_manager.read();
            for id in [super_id, sub_id] {
                let class = cm.get_class(id).expect("class still registered");
                assert!(
                    class_link_failed(class),
                    "attempt {attempt}: {}",
                    class.name
                );
            }
        }
    }

    /// Wave 8, L5: a supertype that failed to link and that another thread
    /// has claimed since, to link it again, is still a LINK failure of the
    /// subtype (not an initialization failure that would make it erroneous).
    #[test]
    fn a_supertype_relinked_by_another_thread_still_failed_to_link() {
        use crate::classloading::ClassLoaderId;
        let shared = test_shared();
        let super_id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                state: LINK_FAILED_STATE,
                initializing_thread: Some(4242),
                ..make_malformed_class("com/example/Relinking", ClassLoaderId::Application)
            });
            id
        };
        assert!(supertype_failed_to_link(&shared, super_id));
        {
            let mut cm = shared.classes.class_manager_write();
            let class = cm.get_class_mut(super_id).expect("registered");
            class.state = ClassState::InitializationError;
            class.initializing_thread = None;
        }
        assert!(!supertype_failed_to_link(&shared, super_id), "erroneous");
    }

    /// Wave 8, L5: many threads racing to initialize a class whose link keeps
    /// failing. A parked link failure is claimable again at once, so a new
    /// claim can land between one claimant's release and its waiter-entry
    /// removal; before the removal moved under the class-manager write lock,
    /// that dropped the old entry un-notified and its waiters slept out the
    /// 30 s timeout. Every attempt must fail fast with the linkage error.
    #[test]
    fn racing_relinks_of_a_link_failing_class_never_strand_a_waiter() {
        use crate::classloading::ClassLoaderId;
        const THREADS: u64 = 8;
        const ATTEMPTS: usize = 200;
        let shared = test_shared();
        let class_id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            let mut c = make_malformed_class("com/example/RacyBoom", ClassLoaderId::Application);
            c.id = id;
            cm.class_store.add(c);
            id
        };
        let barrier = Arc::new(std::sync::Barrier::new(THREADS as usize));
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let shared = shared.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut thread = JvmThread::new(ThreadId(100 + i), "relink-racer");
                    barrier.wait();
                    let mut slowest = std::time::Duration::ZERO;
                    for _ in 0..ATTEMPTS {
                        let start = std::time::Instant::now();
                        let r = ensure_class_initialized_shared(&shared, &mut thread, class_id);
                        slowest = slowest.max(start.elapsed());
                        assert!(
                            matches!(r, Err(MethodCallFailed::InternalError(VmError::Linkage(_)))),
                            "every attempt relinks and rethrows: {r:?}"
                        );
                    }
                    slowest
                })
            })
            .collect();
        for h in handles {
            let slowest = h.join().expect("racer thread");
            assert!(
                slowest < std::time::Duration::from_secs(10),
                "a waiter was stranded until the wait timeout: {slowest:?}"
            );
        }
        let cm = shared.classes.class_manager.read();
        assert!(class_link_failed(
            cm.get_class(class_id).expect("registered")
        ));
        assert!(!shared
            .classes
            .class_init_waiters
            .lock()
            .contains_key(&class_id));
    }

    /// Wave 8, L5: the thread-less link a compile door runs
    /// (`link_class_without_java`) only ever moves a class forward to
    /// `Verified`, and leaves it untouched when a check fails or another
    /// thread holds it.
    #[test]
    fn link_class_without_java_links_forward_or_leaves_the_class_alone() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let add = |name: &str, flags: ClassAccessFlags, loader: ClassLoaderId| {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                access_flags: flags,
                ..make_malformed_class(name, loader)
            });
            id
        };
        let good = add(
            "com/example/LinkGood",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ClassLoaderId::Application,
        );
        let bad = add(
            "com/example/LinkBad",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT,
            ClassLoaderId::Application,
        );
        let trusted = add(
            "java/lang/LinkTrusted",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL | ClassAccessFlags::ABSTRACT,
            ClassLoaderId::Bootstrap,
        );
        let held = add(
            "com/example/LinkHeld",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            ClassLoaderId::Application,
        );
        shared
            .classes
            .class_manager_write()
            .get_class_mut(held)
            .expect("registered")
            .initializing_thread = Some(4242);
        {
            let cm = shared.classes.class_manager.read();
            let linked = |id| class_linked_or_link_trivial(&shared, &cm, cm.get_class(id).unwrap());
            assert!(!linked(good), "never linked, not trivial");
            assert!(!linked(bad), "never linked, not trivial");
            assert!(linked(trusted), "bootstrap-trusted: nothing to check");
        }
        assert!(link_class_without_java(&shared, good));
        assert!(!link_class_without_java(&shared, bad));
        assert!(!link_class_without_java(&shared, held), "claimed elsewhere");
        let cm = shared.classes.class_manager.read();
        let state = |id| {
            let c = cm.get_class(id).expect("registered");
            (c.state, c.initializing_thread)
        };
        assert_eq!(state(good), (ClassState::Verified, None));
        assert_eq!(state(bad), (ClassState::Loaded, None), "left as it was");
        assert_eq!(state(held), (ClassState::Loaded, Some(4242)), "untouched");
        assert!(class_linked_or_link_trivial(
            &shared,
            &cm,
            cm.get_class(good).unwrap()
        ));
        let waiters = shared.classes.class_init_waiters.lock();
        assert!(!waiters.contains_key(&good) && !waiters.contains_key(&bad));
    }

    /// Wave 17 (HotSpot `link_class_impl`): `link_class_on_thread` links the
    /// superclass and the superinterfaces before the class and leaves all of
    /// them `Prepared`, unclaimed and uninitialized; initialization then skips
    /// both link steps, and an interface with no default method stays linked
    /// but uninitialized.
    #[test]
    fn link_class_on_thread_links_the_supertypes_first_and_leaves_them_prepared() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let (super_id, iface_id, sub_id) = {
            let mut cm = shared.classes.class_manager_write();
            let super_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: super_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class("com/example/W17LinkSuper", ClassLoaderId::Application)
            });
            let iface_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: iface_id,
                access_flags: ClassAccessFlags::PUBLIC
                    | ClassAccessFlags::INTERFACE
                    | ClassAccessFlags::ABSTRACT,
                ..make_malformed_class("com/example/W17LinkIface", ClassLoaderId::Application)
            });
            let sub_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: sub_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                superclass: Some(super_id),
                ..make_malformed_class("com/example/W17LinkSub", ClassLoaderId::Application)
            });
            cm.set_interfaces(sub_id, vec![iface_id]);
            (super_id, iface_id, sub_id)
        };
        let mut thread = JvmThread::new(ThreadId(6), "w17-link");
        let linked = link_class_on_thread(&shared, &mut thread, sub_id);
        assert!(linked.is_ok(), "{linked:?}");
        let state = |id: ClassId| {
            let cm = shared.classes.class_manager.read();
            let c = cm.get_class(id).expect("registered");
            (c.state, c.initializing_thread)
        };
        for id in [super_id, iface_id, sub_id] {
            assert_eq!(state(id), (ClassState::Prepared, None), "{id:?}");
            assert!(shared.classes.statics.read().contains_key(&id), "{id:?}");
            assert!(!shared.classes.class_init_waiters.lock().contains_key(&id));
        }
        let again = link_class_on_thread(&shared, &mut thread, sub_id);
        assert!(again.is_ok(), "{again:?}");
        let init = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
        assert!(init.is_ok(), "{init:?}");
        assert_eq!(state(sub_id), (ClassState::Initialized, None));
        assert_eq!(state(super_id), (ClassState::Initialized, None));
        assert_eq!(
            state(iface_id),
            (ClassState::Prepared, None),
            "no default method: linked, never initialized"
        );
    }

    /// gci i22/oo (g4oo item 4): a class linked first (reflection's link, the
    /// VM's exception link) gets its init-path mirror when it is initialized.
    /// Before, `link_claimed_class` returned ahead of the mirror step for a
    /// class already prepared, so its mirror stayed lazy (minted later by a
    /// native that may not collect).
    #[test]
    fn gci_i22oo_a_class_linked_first_gets_its_mirror_at_initialization() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let id = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class("com/example/I22ooLinkedFirst", ClassLoaderId::Application)
            });
            id
        };
        let mut thread = JvmThread::new(ThreadId(7), "i22-oo");
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_MIRROR_AT_INIT", None)],
            || {
                let linked = link_class_on_thread(&shared, &mut thread, id);
                assert!(linked.is_ok(), "{linked:?}");
                assert!(
                    !shared.classes.class_mirrors.read().contains_key(&id),
                    "a link alone makes no mirror"
                );
                let init = ensure_class_initialized_shared(&shared, &mut thread, id);
                assert!(init.is_ok(), "{init:?}");
                assert!(
                    shared.classes.class_mirrors.read().contains_key(&id),
                    "the initialization path made the mirror"
                );
            },
        );
    }

    /// Wave 17: `link_class_on_thread` leaves a class another thread holds
    /// alone, parks a class that fails to link unlinked (retryable), and a
    /// superinterface that fails to link stops its implementor before the
    /// implementor is claimed or prepared, on both entry points.
    #[test]
    fn link_class_on_thread_stops_at_a_held_or_failing_class() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let add = |name: &str, flags: ClassAccessFlags| {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                access_flags: flags,
                ..make_malformed_class(name, ClassLoaderId::Application)
            });
            id
        };
        let held = add(
            "com/example/W17Held",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        // INTERFACE + FINAL fails Pass 2.
        let bad_iface = add(
            "com/example/W17BadIface",
            ClassAccessFlags::PUBLIC
                | ClassAccessFlags::INTERFACE
                | ClassAccessFlags::ABSTRACT
                | ClassAccessFlags::FINAL,
        );
        let implementor = add(
            "com/example/W17Implementor",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        shared
            .classes
            .class_manager_write()
            .set_interfaces(implementor, vec![bad_iface]);
        shared
            .classes
            .class_manager_write()
            .get_class_mut(held)
            .expect("registered")
            .initializing_thread = Some(4242);
        let mut thread = JvmThread::new(ThreadId(7), "w17-link-fail");
        assert!(link_class_on_thread(&shared, &mut thread, held).is_ok());
        let linkage = |r: Result<(), MethodCallFailed>| {
            matches!(r, Err(MethodCallFailed::InternalError(VmError::Linkage(_))))
        };
        assert!(linkage(link_class_on_thread(
            &shared,
            &mut thread,
            implementor
        )));
        {
            let cm = shared.classes.class_manager.read();
            let state = |id| {
                let c = cm.get_class(id).expect("registered");
                (c.state, c.initializing_thread)
            };
            assert_eq!(state(held), (ClassState::Loaded, Some(4242)), "untouched");
            assert!(class_link_failed(
                cm.get_class(bad_iface).expect("registered")
            ));
            assert_eq!(
                state(implementor),
                (ClassState::Loaded, None),
                "stopped before it was claimed"
            );
        }
        assert!(linkage(ensure_class_initialized_shared(
            &shared,
            &mut thread,
            implementor
        )));
        let cm = shared.classes.class_manager.read();
        assert!(class_link_failed(
            cm.get_class(implementor).expect("registered")
        ));
        assert!(!shared.classes.statics.read().contains_key(&implementor));
        let waiters = shared.classes.class_init_waiters.lock();
        assert!(!waiters.contains_key(&bad_iface) && !waiters.contains_key(&implementor));
    }

    /// Wave 17 (`interpreter-L1-rust-jvmti-manager-class-prepare-fires-at-define-time-FIXED`):
    /// the VM's own Rust-side JVMTI manager hears `ClassPrepare` at the
    /// prepare point, on the preparing thread, superclass first, once per
    /// class (a second initialization, and a second post of the same class,
    /// report nothing).
    #[test]
    fn the_vm_jvmti_manager_hears_class_prepare_once_on_the_preparing_thread() {
        use crate::classloading::ClassLoaderId;
        use crate::runtime::jvmti::{self as rt, EventCallbacks, EventMode, JvmtiEventKind};
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let (super_id, sub_id) = {
            let mut cm = shared.classes.class_manager_write();
            let super_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: super_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class("java/lang/W17RustPrepareSuper", ClassLoaderId::Bootstrap)
            });
            let sub_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: sub_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                superclass: Some(super_id),
                ..make_malformed_class("java/lang/W17RustPrepareSub", ClassLoaderId::Bootstrap)
            });
            (super_id, sub_id)
        };
        let wire = |id: ClassId| u64::from(id.as_u32());
        let seen: Arc<std::sync::Mutex<Vec<(u64, u64)>>> = Arc::default();
        let manager = rt::manager_for_vm(shared.vm_identity)
            .expect("SharedVm::new installs the VM's manager");
        let sink = Arc::clone(&seen);
        manager
            .set_event_callbacks(EventCallbacks {
                class_prepare: Some(Box::new(move |thread, class| {
                    if let Ok(mut seen) = sink.lock() {
                        seen.push((thread, class));
                    }
                })),
                ..EventCallbacks::default()
            })
            .expect("callbacks");
        manager
            .set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassPrepare, None)
            .expect("enable");
        let mut thread = JvmThread::new(ThreadId(9), "w17-rust-prepare");
        let first = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
        let second = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
        rt::fire_class_prepare_for_vm(shared.vm_identity, 9, wire(sub_id));
        let mut got = seen.lock().map(|s| s.clone()).unwrap_or_default();
        rt::forget_vm_jvmti_state(shared.vm_identity);
        assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
        got.retain(|&(_, class)| class == wire(super_id) || class == wire(sub_id));
        assert_eq!(got, vec![(9, wire(super_id)), (9, wire(sub_id))]);
    }

    /// Wave 18: reflection links a never-linked class in every mode,
    /// `--compatible` included, and leaves it prepared and unclaimed.
    #[test]
    fn reflection_links_the_class_in_every_mode() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        assert!(!shared.config.is_jdk_only());
        assert!(reflection_links_class_enforced(&shared));
        let class = {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class("com/example/W17Reflected", ClassLoaderId::Application)
            });
            id
        };
        let mut thread = JvmThread::new(ThreadId(8), "w17-reflect");
        assert!(link_class_for_reflection(&shared, &mut thread, class).is_ok());
        let cm = shared.classes.class_manager.read();
        let c = cm.get_class(class).expect("registered");
        assert_eq!(
            (c.state, c.initializing_thread),
            (ClassState::Prepared, None)
        );
    }

    /// JVMS §5.5 step 7 / HotSpot `initialize_super_interfaces` (interpreter
    /// round i1 wave 9, lane L5): a class initializes, in recursive
    /// interfaces-array order, exactly the superinterfaces that DECLARE a
    /// default method — not one that only inherits it — and an interface
    /// initializes none.
    #[test]
    fn a_class_initializes_only_the_superinterfaces_that_declare_a_default() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
        use cratonvm_reader::method::ClassFileMethod;
        let method = |name: &str, flags: MethodAccessFlags| ClassFileMethod {
            access_flags: flags,
            name: Arc::from(name),
            descriptor: Arc::from("()V"),
            attributes: Vec::new(),
        };
        let shared = test_shared();
        let mut cm = shared.classes.class_manager_write();
        let mut add = |name: &str, interface: bool, methods: Vec<ClassFileMethod>| {
            let id = cm.class_store.next_id();
            let access_flags = if interface {
                ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT
            } else {
                ClassAccessFlags::PUBLIC
            };
            cm.class_store.add(Class {
                id,
                access_flags,
                methods,
                ..make_malformed_class(name, ClassLoaderId::Application)
            });
            id
        };
        let declares = add(
            "p/Declares",
            true,
            vec![method("d", MethodAccessFlags::PUBLIC)],
        );
        let inherits = add(
            "p/Inherits",
            true,
            vec![method(
                "a",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            )],
        );
        let statics_only = add(
            "p/StaticsOnly",
            true,
            vec![method(
                "s",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            )],
        );
        let other = add(
            "p/Other",
            true,
            vec![method("o", MethodAccessFlags::PUBLIC)],
        );
        let class = add("p/C", false, Vec::new());
        let twice = add("p/Twice", false, Vec::new());
        cm.set_interfaces(inherits, vec![declares]);
        cm.set_interfaces(class, vec![inherits, statics_only, other]);
        cm.set_interfaces(twice, vec![declares, inherits]);

        assert_eq!(
            superinterfaces_to_initialize(&cm, class),
            vec![declares, other],
            "the inheriting interface is recursed into but not initialized"
        );
        assert_eq!(superinterfaces_to_initialize(&cm, twice), vec![declares]);
        assert!(
            superinterfaces_to_initialize(&cm, inherits).is_empty(),
            "an interface's initialization initializes no superinterface"
        );
    }

    /// Interpreter round i1 wave 10, lane L4 (JVMS §5.5 step 2 for step 7): a
    /// class whose superinterface another thread is initializing waits for
    /// that initialization to complete instead of skipping the interface.
    #[test]
    fn a_class_waits_for_a_superinterface_another_thread_is_initializing() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
        use cratonvm_reader::method::ClassFileMethod;
        let shared = test_shared();
        let (iface, class) = {
            let mut cm = shared.classes.class_manager_write();
            let iface = cm.class_store.next_id();
            // Bootstrap-trusted names: verification is skipped, so the test
            // exercises only the initialization protocol.
            cm.class_store.add(Class {
                id: iface,
                access_flags: ClassAccessFlags::PUBLIC
                    | ClassAccessFlags::INTERFACE
                    | ClassAccessFlags::ABSTRACT,
                methods: vec![ClassFileMethod {
                    access_flags: MethodAccessFlags::PUBLIC,
                    name: Arc::from("d"),
                    descriptor: Arc::from("()V"),
                    attributes: Vec::new(),
                }],
                state: ClassState::Initializing,
                initializing_thread: Some(4242),
                ..make_malformed_class("java/lang/InitWaitIface", ClassLoaderId::Bootstrap)
            });
            let class = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: class,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class("java/lang/InitWaitImpl", ClassLoaderId::Bootstrap)
            });
            cm.set_interfaces(class, vec![iface]);
            (iface, class)
        };
        shared.classes.class_init_waiters.lock().insert(
            iface,
            Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new())),
        );
        let initializer = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut thread = JvmThread::new(ThreadId(77), "init-waiter");
                ensure_class_initialized_shared(&shared, &mut thread, class)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !initializer.is_finished(),
            "the class finished initializing while its superinterface was still initializing"
        );
        assert_ne!(
            shared
                .classes
                .class_manager
                .read()
                .get_class(class)
                .map(|c| c.state),
            Some(ClassState::Initialized)
        );
        assert!(finalize_class_init(
            &shared,
            iface,
            ClassState::Initialized,
            4242
        ));
        let result = initializer.join().expect("initializer thread");
        assert!(result.is_ok(), "{result:?}");
        let cm = shared.classes.class_manager.read();
        assert_eq!(
            cm.get_class(class).map(|c| c.state),
            Some(ClassState::Initialized)
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

    /// The wire class ids of the `ClassPrepare` events queued on `rx`, in
    /// order (the event's data is tag, class id, signature, status).
    #[cfg(feature = "experimental-debug")]
    fn class_prepare_events(rx: &std::sync::mpsc::Receiver<crate::debug::DebugEvent>) -> Vec<u64> {
        rx.try_iter()
            .filter(|e| e.kind == crate::debug::events::EventKind::ClassPrepare)
            .map(|e| {
                let mut id = [0u8; 8];
                id.copy_from_slice(&e.extra[1..9]);
                u64::from_be_bytes(id)
            })
            .collect()
    }

    /// A VM with one JDWP `ClassPrepare` request (no filter, no suspension)
    /// and its event channel.
    #[cfg(feature = "experimental-debug")]
    fn class_prepare_listener(
        shared: &SharedVm,
    ) -> std::sync::mpsc::Receiver<crate::debug::DebugEvent> {
        use crate::debug::events::{EventKind, SuspendPolicy};
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.events
                .set_event_request(EventKind::ClassPrepare, SuspendPolicy::None, vec![]);
            crate::debug::publish_debugger_gates(shared, &ds);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        *shared.debug.debug_event_tx.lock().expect("event channel") = Some(tx);
        rx
    }

    /// Wave 15 (`interpreter-L1-jvmti-class-prepare-is-reported-at-initialization-not-at-linking-FIXED`):
    /// a class is prepared, and reported, as part of its linking; wave 17
    /// (HotSpot `link_class_impl`) links the superclass first, so the
    /// superclass's `ClassPrepare` comes first, and a later initialization
    /// request reports nothing again.
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn class_prepare_reports_the_superclass_before_the_subclass() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let (super_id, sub_id) = {
            let mut cm = shared.classes.class_manager_write();
            let super_id = cm.class_store.next_id();
            // Bootstrap-trusted names: verification is skipped.
            cm.class_store.add(Class {
                id: super_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class("java/lang/W15PrepareSuper", ClassLoaderId::Bootstrap)
            });
            let sub_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: sub_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                superclass: Some(super_id),
                ..make_malformed_class("java/lang/W15PrepareSub", ClassLoaderId::Bootstrap)
            });
            (super_id, sub_id)
        };
        let rx = class_prepare_listener(&shared);
        let mut thread = JvmThread::new(ThreadId(5), "w15-prepare");
        let result = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
        assert!(result.is_ok(), "{result:?}");
        let wire = |id: ClassId| u64::from(id.as_u32());
        assert_eq!(
            class_prepare_events(&rx),
            vec![wire(super_id), wire(sub_id)],
            "the superclass is linked, prepared and reported before its subclass"
        );
        let again = ensure_class_initialized_shared(&shared, &mut thread, sub_id);
        assert!(again.is_ok(), "{again:?}");
        assert!(class_prepare_events(&rx).is_empty(), "once");
    }

    /// Wave 17 (HotSpot `link_class_impl`): a class whose superclass fails to
    /// link is never prepared nor reported, however often its
    /// initialization is retried (wave 15 prepared it first, and reported it
    /// once).
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn a_class_whose_superclass_fails_to_link_is_never_prepared() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let sub_id = {
            let mut cm = shared.classes.class_manager_write();
            let super_id = cm.class_store.next_id();
            // As in `a_superclass_link_failure_leaves_the_subclass_retryable`:
            // ANNOTATION without INTERFACE fails Pass 2.
            cm.class_store.add(Class {
                id: super_id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::ANNOTATION,
                ..make_malformed_class("com/example/W15BadSuper", ClassLoaderId::Application)
            });
            let sub_id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id: sub_id,
                access_flags: ClassAccessFlags::PUBLIC,
                superclass: Some(super_id),
                ..make_malformed_class("com/example/W15GoodSub", ClassLoaderId::Application)
            });
            sub_id
        };
        let rx = class_prepare_listener(&shared);
        let mut thread = JvmThread::new(ThreadId(5), "w15-prepare-retry");
        for attempt in 0..2 {
            assert!(
                ensure_class_initialized_shared(&shared, &mut thread, sub_id).is_err(),
                "attempt {attempt}: the superclass fails to link"
            );
        }
        assert!(
            class_prepare_events(&rx).is_empty(),
            "neither the superclass nor the subclass is prepared"
        );
        let cm = shared.classes.class_manager.read();
        assert!(class_link_failed(cm.get_class(sub_id).expect("registered")));
        assert!(
            !shared.classes.statics.read().contains_key(&sub_id),
            "no statics block: the subclass was never prepared"
        );
    }

    /// Wave 15: while `ClassPrepare` is observed, a compile door neither
    /// links a class early (`link_class_without_java` never prepares) nor
    /// counts a never-linked class as trivially linked, so the class is
    /// initialized — prepared and reported — on a Java thread.
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn a_class_is_not_linked_early_while_class_prepare_is_observed() {
        use crate::classloading::ClassLoaderId;
        use cratonvm_reader::class_access_flags::ClassAccessFlags;
        let shared = test_shared();
        let add = |name: &str, loader: ClassLoaderId| {
            let mut cm = shared.classes.class_manager_write();
            let id = cm.class_store.next_id();
            cm.class_store.add(Class {
                id,
                access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
                ..make_malformed_class(name, loader)
            });
            id
        };
        let good = add("com/example/W15EarlyLink", ClassLoaderId::Application);
        let trusted = add("java/lang/W15EarlyTrusted", ClassLoaderId::Bootstrap);
        let gates = &shared.debug.debugger_gates;
        gates.set_native_class_prepare(true);
        assert!(class_prepare_observed(&shared));
        assert!(!link_class_without_java(&shared, good));
        {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(good).expect("registered");
            assert_eq!(
                (class.state, class.initializing_thread),
                (ClassState::Loaded, None)
            );
            assert!(!class_linked_or_link_trivial(
                &shared,
                &cm,
                cm.get_class(trusted).expect("registered")
            ));
        }
        gates.set_native_class_prepare(false);
        assert!(!class_prepare_observed(&shared));
        assert!(link_class_without_java(&shared, good));
        let cm = shared.classes.class_manager.read();
        assert!(class_linked_or_link_trivial(
            &shared,
            &cm,
            cm.get_class(trusted).expect("registered")
        ));
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
            method_descriptor: None,
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

#[cfg(test)]
mod gcg_g1o_class_prep_reclaim_tests {
    use super::prep_refusal_may_reclaim;
    use crate::error::{RuntimeError, VmError};

    /// gcg g1b/o: only a heap refusal on the initialization path collects
    /// and retries; `=0` restores the error.
    #[test]
    fn gcg_g1o_only_an_initializing_heap_refusal_reclaims() {
        let oome = VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Java heap space (prepare_class C.F ConstantValue String)".to_string(),
        });
        let other = VmError::Internal {
            message: "prepare_class: class 7 not found".to_string(),
        };
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_CLASS_PREP_RECLAIM", None)],
            || {
                assert!(prep_refusal_may_reclaim(&oome, true));
                assert!(!prep_refusal_may_reclaim(&oome, false));
                assert!(!prep_refusal_may_reclaim(&other, true));
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_CLASS_PREP_RECLAIM", Some("0"))],
            || assert!(!prep_refusal_may_reclaim(&oome, true)),
        );
    }
}

#[cfg(test)]
mod gcg_g3o_mirror_at_init_tests {
    use super::{mirror_at_init_enabled, mirror_at_init_flag, SharedVm};

    /// gcg g3/o: the init-path mirror is opt-in (`runtime_flag_on`).
    #[test]
    fn gcg_g3o_the_init_path_mirror_is_opt_in() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_MIRROR_AT_INIT", None)],
            || assert!(mirror_at_init_flag()),
        );
        // gcg g6/oo: flipped to default ON; `=0` is the kill switch (the
        // test keeps its name for the round's filters).
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_MIRROR_AT_INIT", Some("0"))],
            || assert!(!mirror_at_init_flag()),
        );
    }

    /// gcg g6/oo: the switch is read once per VM; a later override does not
    /// change a VM that already answered, and a new VM reads it afresh.
    #[test]
    fn gcg_g6oo_the_init_path_mirror_switch_is_cached_per_vm() {
        let on = SharedVm::new(crate::config::VmConfig::default());
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_MIRROR_AT_INIT", None)],
            || assert!(mirror_at_init_enabled(&on)),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_MIRROR_AT_INIT", Some("0"))],
            || {
                assert!(mirror_at_init_enabled(&on), "cached for this VM");
                let off = SharedVm::new(crate::config::VmConfig::default());
                assert!(!mirror_at_init_enabled(&off), "a new VM reads afresh");
            },
        );
    }

    /// The init-path mirror runs after `ClassPrepare` and only on the
    /// initialization path (`failure_state` set), where a collection is
    /// allowed; the plain link path (`link_class_on_thread`) never collects.
    #[test]
    fn gcg_g3o_the_init_path_mirror_is_gated_on_the_initialization_path() {
        let src = include_str!("vm_util.rs");
        let body = src
            .split("fn link_claimed_class(")
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("link_claimed_class body");
        let prepare = body
            .find("fire_class_prepare_for_vm(")
            .expect("ClassPrepare report");
        let gate = body
            .find("if failure_state.is_some() && mirror_at_init_enabled(shared)")
            .expect("the init-path gate");
        assert!(prepare < gate, "the mirror is made after ClassPrepare");
    }
}

#[cfg(test)]
mod gcg_g4oo_init_path_collect_tests {
    use super::{init_path_collect_safe, init_path_may_collect, ClassId, SharedVm};

    /// gcg g4/oo: outside a native callback the init path may collect without
    /// asking about `<clinit>`; inside one only when Java runs anyway.
    #[test]
    fn gcg_g4oo_init_path_collects_outside_a_native_or_when_java_runs() {
        let asked = std::cell::Cell::new(false);
        assert!(init_path_collect_safe(false, || {
            asked.set(true);
            false
        }));
        assert!(!asked.get(), "not asked outside a native");
        assert!(init_path_collect_safe(true, || true));
        assert!(!init_path_collect_safe(true, || false));
    }

    /// The native scope is the one `native_oom` grants: a class with no
    /// `<clinit>` (here: not loaded at all) is refused inside it only.
    #[test]
    fn gcg_g4oo_a_native_scope_without_a_clinit_does_not_collect() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let absent = ClassId::new(0x7fff_fff0);
        assert!(init_path_may_collect(&shared, absent));
        let inside =
            crate::runtime::native_oom::catch_alloc_oom(|| init_path_may_collect(&shared, absent));
        assert!(matches!(inside, Ok(false)));
    }

    /// Both init-path collections ask the guard before they collect.
    #[test]
    fn gcg_g4oo_both_init_path_reclaims_are_guarded() {
        let src = include_str!("vm_util.rs");
        for head in ["fn link_claimed_class(", "fn create_mirror_on_init_path("] {
            let body = src
                .split(head)
                .nth(1)
                .and_then(|rest| rest.split("\nfn ").next())
                .expect("function body");
            let guard = body
                .find("init_path_may_collect(shared, class_id)")
                .expect("the guard");
            let reclaim = body
                .find("native_reclaim_before_alloc_retry_graced(")
                .expect("the reclaim");
            assert!(guard < reclaim, "{head}: the guard precedes the reclaim");
        }
    }
}

#[cfg(test)]
mod gci_i22oo_reflective_link_reclaim_tests {
    use super::reflection_link_refusal_may_reclaim;
    use crate::error::{LinkageError, MethodCallFailed, RuntimeError, VmError};
    use cratonvm_types::ObjectRef;

    fn heap_refusal() -> MethodCallFailed {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Java heap space (prepare_class C.F ConstantValue String)".to_string(),
        }))
    }

    /// gci i22/oo (g4oo item 5): only the heap's refusal of a reflective link
    /// collects and links again; a thrown Java error, a linkage error and the
    /// `CRATONVM_GC_CLASS_PREP_RECLAIM=0` kill keep the first answer.
    #[test]
    fn gci_i22oo_only_a_heap_refusal_of_a_reflective_link_reclaims() {
        // SAFETY: non-null, 8-byte aligned, never dereferenced.
        let thrown = MethodCallFailed::ExceptionThrown(unsafe {
            ObjectRef::from_raw(0x1000usize as *mut u8)
        });
        let linkage = MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoClassDefFoundError {
                class_name: "p/Missing".to_string(),
            },
        ));
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_CLASS_PREP_RECLAIM", None)],
            || {
                assert!(reflection_link_refusal_may_reclaim(&heap_refusal()));
                assert!(!reflection_link_refusal_may_reclaim(&thrown));
                assert!(!reflection_link_refusal_may_reclaim(&linkage));
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_CLASS_PREP_RECLAIM", Some("0"))],
            || assert!(!reflection_link_refusal_may_reclaim(&heap_refusal())),
        );
    }

    /// The enforced reflective link goes through the reclaiming door, and the
    /// door asks its rule before it collects.
    #[test]
    fn gci_i22oo_the_reflective_link_asks_before_it_collects() {
        let src = include_str!("vm_util.rs").replace("\r\n", "\n");
        let entry = src
            .split("pub fn link_class_for_reflection(")
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("link_class_for_reflection body");
        assert!(entry.contains(
            "return link_class_for_reflection_reclaiming(shared, thread, class_id);"
        ));
        let body = src
            .split("fn link_class_for_reflection_reclaiming(")
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("link_class_for_reflection_reclaiming body");
        let rule = body
            .find("reflection_link_refusal_may_reclaim(&err)")
            .expect("the rule");
        let reclaim = body
            .find("native_reclaim_before_alloc_retry_graced(")
            .expect("the collection");
        let relink = body
            .rfind("link_class_on_thread(shared, thread, class_id)")
            .expect("the second link");
        assert!(rule < reclaim && reclaim < relink);
    }

    /// The collection is safe only because every caller of
    /// `NativeContext::link_class_for_reflection` holds no heap reference it
    /// reads again across it. Exactly the three reflective listings call it;
    /// a new caller must be checked for that and counted here.
    #[test]
    fn gci_i22oo_the_reflective_link_callers_hold_no_reference_across_it() {
        let lang_class =
            include_str!("../../../native-builtins/src/lang_class.rs").replace("\r\n", "\n");
        assert_eq!(lang_class.matches("ctx.link_class_for_reflection(").count(), 3);
        for head in [
            "pub(crate) fn native_class_get_declared_fields(",
            "pub(crate) fn native_class_get_declared_methods(",
            "pub(crate) fn native_class_get_declared_constructors(",
        ] {
            let body = lang_class
                .split(head)
                .nth(1)
                .and_then(|rest| rest.split("\n\u{7d}\n").next())
                .expect("listing body");
            assert_eq!(
                body.matches("ctx.link_class_for_reflection(").count(),
                1,
                "{head}"
            );
        }
        let lib = include_str!("../../../native-builtins/src/lib.rs");
        assert_eq!(lib.matches("ctx.link_class_for_reflection(").count(), 0);
    }
}
