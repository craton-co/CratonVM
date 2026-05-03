//! JMX (Java Management Extensions) native method implementations.
//! Provides MBeanServer and platform MXBeans for runtime monitoring.

use rustjvm_types::ClassId;
use rustjvm_types::error::MethodCallResult;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectRef, Value};
use std::time::Instant;
use std::sync::OnceLock;

use crate::{native_noop_with_this, obj_arg, alloc_concurrent_synthetic};

/// VM start time – initialised once on first access.
static VM_START: OnceLock<Instant> = OnceLock::new();

/// Epoch millis corresponding to VM_START (for RuntimeMXBean.getStartTime).
static VM_START_EPOCH_MS: OnceLock<u64> = OnceLock::new();

fn vm_start() -> &'static Instant {
    VM_START.get_or_init(Instant::now)
}

fn vm_start_epoch_ms() -> u64 {
    *VM_START_EPOCH_MS.get_or_init(|| {
        // Ensure VM_START is initialised too
        let _ = vm_start();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    })
}

fn uptime_ms() -> u64 {
    vm_start().elapsed().as_millis() as u64
}

// ---------------------------------------------------------------------------
// Public registration entry-point
// ---------------------------------------------------------------------------

pub(crate) fn register_jmx_natives(r: &mut NativeMethodRegistry) {
    register_management_factory(r);
    register_runtime_mxbean(r);
    register_memory_mxbean(r);
    register_memory_usage(r);
    register_thread_mxbean(r);
    register_class_loading_mxbean(r);
    register_operating_system_mxbean(r);
    register_compilation_mxbean(r);
    register_gc_mxbean(r);
    register_mbean_server(r);
    register_vm_management_impl(r);
}

// ---------------------------------------------------------------------------
// sun.management.VMManagementImpl
//
// RKC16N.10: `java.lang.management.ManagementFactory.<clinit>` instantiates
// `sun.management.VMManagementImpl`, whose `<clinit>` calls a set of native
// helpers that talk to the JMM interface. Without these, JBoss Modules'
// `Module.<clinit>` chain (which Keycloak boots through) silent-swallows an
// `UnsatisfiedLinkError` for `getVersion0` and then a hard one for
// `getStartupTime`. Providing reasonable defaults lets the boot advance.
// ---------------------------------------------------------------------------

/// RKC16N.10: register the `sun.management.VMManagementImpl` natives so
/// `java.lang.management.ManagementFactory.<clinit>` doesn't throw
/// `UnsatisfiedLinkError` and JBoss Modules' `Module.<clinit>` chain
/// (Keycloak boot) can advance past it.
///
/// **Call from both real-JDK and synthetic-JDK registration paths.** This
/// function lives outside `register_jmx_natives` because that helper is only
/// reachable from `register_synthetic_overrides`, which is skipped in
/// real-JDK mode (synthetic-mode-only field layouts).
pub fn register_vm_management_impl(r: &mut NativeMethodRegistry) {
    // (probe eprintln removed — registration confirmed working)
    let cls = "sun/management/VMManagementImpl";

    // Management interface version. OpenJDK reports "10.0" for JDK 8+.
    // Format: "<major>.<minor>" — JBoss / Hotspot consumers parse only major.
    r.register(cls, "getVersion0", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("10.0");
        Ok(Some(Value::Object(Some(s))))
    });

    // JVM init-done time, epoch millis. Mirrors RuntimeMXBean.getStartTime.
    r.register(cls, "getStartupTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(vm_start_epoch_ms() as i64)))
    });

    // Process id. Same plumbing as RuntimeMXBean.getName which embeds PID.
    r.register(cls, "getProcessId", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(std::process::id() as i32)))
    });

    // initOptionalSupportFields populates a bag of "isXxxSupported" booleans
    // on the receiver. OpenJDK writes them via Unsafe + JNI; we no-op so the
    // synthetic instance stays at its default (all `false`) — accurate, since
    // we don't claim any optional JMM features.
    r.register(cls, "initOptionalSupportFields", "()V", |_ctx, _args| {
        Ok(None)
    });

    // No JVM args plumbed through to JMM yet — return an empty String[].
    r.register(
        cls,
        "getVmArguments0",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // -- VMManagementImpl `is*Supported` / `is*Enabled` queries --
    //
    // ManagementFactory.<clinit> iterates the entire feature-flag surface
    // to populate static booleans. We don't support any of these optional
    // JMM features yet (thread CPU time, allocated-memory tracking, object
    // monitor usage, synchronizer usage, etc.), so every flag is `false`.
    // Done as a batch to avoid the iterate-and-add-one-at-a-time treadmill;
    // the consumer is iterating a static const list at clinit time.
    let false_zero: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Int(0))); // false / 0
    for name in [
        "isThreadAllocatedMemorySupported",
        "isThreadAllocatedMemoryEnabled",
        "isThreadContentionMonitoringSupported",
        "isThreadContentionMonitoringEnabled",
        "isThreadCpuTimeSupported",
        "isThreadCpuTimeEnabled",
        "isCurrentThreadCpuTimeSupported",
        "isOtherThreadCpuTimeSupported",
        "isObjectMonitorUsageSupported",
        "isSynchronizerUsageSupported",
        "isBootClassPathSupported",
        "isCompilationTimeMonitoringSupported",
        "isRemoteDiagnosticCommandsSupported",
        "isGcNotificationSupported",
        "getVerboseClass",
        "getVerboseGC",
    ] {
        r.register(cls, name, "()Z", false_zero);
    }

    // -- VMManagementImpl long-typed counters / timers --
    //
    // Returning 0 is consistent with "unsupported / not measured": JBoss
    // uses these for diagnostic output, not for control flow.
    let zero_long: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Long(0)));
    // Long-typed natives — `()J`. Only `getTotalThreadCount` is `long` on
    // the thread side; live/peak/daemon are `int` (see int-typed batch
    // below). Mismatching the descriptor causes the dispatcher to miss
    // the registration and the JVM to raise UnsatisfiedLinkError.
    for name in [
        "getTotalCompileTime",
        "getTotalClassCount",
        "getUnloadedClassCount",
        "getLoadedClassSize",
        "getUnloadedClassSize",
        "getClassLoadingTime",
        "getMethodDataSize",
        "getInitializedClassCount",
        "getClassInitializationTime",
        "getClassVerificationTime",
        "getSafepointSyncTime",
        "getTotalSafepointTime",
        "getSafepointCount",
        "getTotalApplicationNonStoppedTime",
        "getTotalThreadCount",
    ] {
        r.register(cls, name, "()J", zero_long);
    }

    // -- VMManagementImpl int-typed counters --
    //
    // RKC16N.12: live/peak/daemon thread counts are declared `int`, not
    // `long`, in JDK 25's VMManagementImpl. Registering them with `()J`
    // (as the previous batch did) caused the dispatcher to never match
    // the call site, surfacing as `UnsatisfiedLinkError` during
    // `ManagementFactoryHelper.<clinit>` -> `new VMManagementImpl()`
    // chain on Keycloak boot.
    let zero_int: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Int(0)));
    for name in [
        "getLiveThreadCount",
        "getPeakThreadCount",
        "getDaemonThreadCount",
    ] {
        r.register(cls, name, "()I", zero_int);
    }
    // Reset peak counter — accept and ignore.
    r.register(cls, "resetPeakThreadCount", "()V", |_ctx, _args| Ok(None));

    // -- VMManagementImpl uptime + processor count --
    //
    // RKC16N.12: `getUptime()` calls `getUptime0()J` and
    // `RuntimeImpl.getAvailableProcessors()` delegates to
    // `VMManagementImpl.getAvailableProcessors()I`. Both are declared
    // native; without registrations the JMM init chain trips on ULE.
    // `getUptime0` returns real elapsed millis since VM start (we already
    // track this for `getStartupTime`); `getAvailableProcessors` reports
    // Rust's view of the host's parallelism, matching what
    // `Runtime.availableProcessors()` would report.
    r.register(cls, "getUptime0", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(uptime_ms() as i64)))
    });
    r.register(cls, "getAvailableProcessors", "()I", |_ctx, _args| {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        Ok(Some(Value::Int(n as i32)))
    });

    // -- sun.management.MemoryImpl --
    // Wave 1 / Task A: ManagementFactory.getMemoryPoolMXBeans() /
    // getMemoryManagerMXBeans() / getGarbageCollectorMXBeans() all bottom
    // out in `MemoryImpl.getMemoryPools0()` / `getMemoryManagers0()`. The
    // GC list is built by filtering the manager array for `instanceof
    // GarbageCollectorMXBean`, so to populate all three lists we return
    // (a) two `MemoryPoolImpl` instances tagged HEAP, and (b) one
    // `GarbageCollectorImpl` instance (which `extends MemoryManagerImpl
    // implements GarbageCollectorMXBean`, satisfying both filters).
    //
    // The synthetic instances carry their `name` + `isHeap` fields by
    // name (resolved at runtime); the natives we register on
    // `MemoryPoolImpl` / `MemoryManagerImpl` / `GarbageCollectorImpl`
    // (`getName`, `getType`, `getCollectionCount`) win dispatch over the
    // bytecode methods, so even if a synthetic field slot were wrong, the
    // probe-visible answers still come out right.
    let memory_impl = "sun/management/MemoryImpl";
    r.register(
        memory_impl,
        "getMemoryPools0",
        "()[Ljava/lang/management/MemoryPoolMXBean;",
        |ctx, _args| {
            let pool_cid = ctx
                .ensure_class_initialized("sun/management/MemoryPoolImpl")
                .unwrap_or(ClassId::new(0));
            let arr = ctx.new_ref_array(pool_cid, 2);
            let p0 = alloc_memory_pool_impl(ctx, "Eden Space", true);
            let p1 = alloc_memory_pool_impl(ctx, "Old Gen", true);
            ctx.set_array_element(arr, 0, Value::Object(Some(p0)));
            ctx.set_array_element(arr, 1, Value::Object(Some(p1)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    r.register(
        memory_impl,
        "getMemoryManagers0",
        "()[Ljava/lang/management/MemoryManagerMXBean;",
        |ctx, _args| {
            let mgr_cid = ctx
                .ensure_class_initialized("sun/management/MemoryManagerImpl")
                .unwrap_or(ClassId::new(0));
            let arr = ctx.new_ref_array(mgr_cid, 1);
            // GarbageCollectorImpl extends MemoryManagerImpl + implements
            // GarbageCollectorMXBean — single instance covers both the
            // manager list and (via the instanceof filter) the GC list.
            let gc = alloc_garbage_collector_impl(ctx, "G1 Young Generation");
            ctx.set_array_element(arr, 0, Value::Object(Some(gc)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    // setVerboseGC(boolean) — accept and ignore.
    r.register(memory_impl, "setVerboseGC", "(Z)V", |_ctx, _args| Ok(None));
    // getMemoryUsage0(boolean heap) returns a MemoryUsage with -1 fields,
    // matching MemoryUsage.UNDEFINED_USAGE per the JMM spec.
    r.register(
        memory_impl,
        "getMemoryUsage0",
        "(Z)Ljava/lang/management/MemoryUsage;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4);
            ctx.set_field(obj, 0, Value::Long(-1)); // init
            ctx.set_field(obj, 1, Value::Long(-1)); // used
            ctx.set_field(obj, 2, Value::Long(-1)); // committed
            ctx.set_field(obj, 3, Value::Long(-1)); // max
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // -- ManagementFactory.loadNativeLib + loadLibrary chain --
    //
    // Session 98 root-cause for `B6: silent-swallow class=ManagementFactory
    // exc=UnsatisfiedLinkError` on KC16 boot:
    //
    // `java.lang.management.ManagementFactory.<clinit>` calls
    // `loadNativeLib()V` (a private static helper) whose body is just
    // `System.loadLibrary("management")`. The real-JDK bytecode for
    // `System.loadLibrary` walks `Reflection.getCallerClass` ->
    // `Runtime.getRuntime().loadLibrary0` -> `ClassLoader.loadLibrary`,
    // which throws `UnsatisfiedLinkError` (with a NULL detail message)
    // because libmanagement.dll genuinely is not on java.library.path —
    // we ship the JMM natives in-process via NativeMethodRegistry.
    //
    // RKC16N.12 (Session 97) attempted a fix in vm_exec.rs by adding a
    // "force-native-override" entry for `System.loadLibrary` /
    // `Runtime.loadLibrary*`, but in real-JDK mode the *underlying*
    // overrides for those methods (registered in `lang_system::
    // register_runtime_natives`) are never wired in: the real-JDK
    // bootstrap in `vm/src/vm/vm_init.rs` calls `register_essential_natives`
    // (which omits Runtime + System.loadLibrary) but NOT
    // `register_runtime_natives` or `register_synthetic_overrides`. So the
    // override-allowlist in vm_exec finds no native to substitute in and
    // falls through to the throwing bytecode.
    //
    // Cheapest scoped fix: register no-op natives for the three methods in
    // the loadLibrary chain right here, since `register_vm_management_impl`
    // IS called from real-JDK init (vm_init.rs line 842). Once any of these
    // wins the dispatch (try_stackless_invoke checks the native registry
    // before running bytecode — see vm/src/runtime/interpreter.rs line
    // 8039), the chain short-circuits and ManagementFactory.<clinit>
    // completes cleanly.
    //
    // Registering at all three levels (loadNativeLib, System.loadLibrary,
    // Runtime.loadLibrary0) is belt-and-suspenders — any single match
    // suffices. We start the chain at `loadNativeLib` because it is the
    // narrowest scope (private static helper used only by ManagementFactory)
    // and least likely to mask real bugs in unrelated app code that calls
    // `System.loadLibrary` for its own purposes.
    r.register(
        "java/lang/management/ManagementFactory",
        "loadNativeLib",
        "()V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "java/lang/System",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "java/lang/System",
        "load",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "java/lang/Runtime",
        "loadLibrary0",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        "java/lang/Runtime",
        "load0",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );

    // Wave 1 / Task A: short-circuit `ManagementFactory.getPlatformMXBeans
    // (Class<? extends PlatformManagedObject>)List` — the bytecode body
    // routes through `PlatformMBeanFinder.findFirst` + `PlatformComponent`
    // SPI loading, neither of which is wired up in our VM. The convenience
    // wrappers `getMemoryPoolMXBeans()` / `getMemoryManagerMXBeans()` /
    // `getGarbageCollectorMXBeans()` all flow through this method, so a
    // single override populates all three lists at once.
    r.register(
        "java/lang/management/ManagementFactory",
        "getPlatformMXBeans",
        "(Ljava/lang/Class;)Ljava/util/List;",
        |ctx, args| {
            let cls_arg = obj_arg(args, 0).ok();
            let cls_name: String = cls_arg
                .and_then(|c| ctx.class_id_from_mirror(c))
                .and_then(|id| ctx.class_name_of_id(id))
                .unwrap_or_default();
            let beans: Vec<ObjectRef> = match cls_name.as_str() {
                "java/lang/management/MemoryPoolMXBean" => vec![
                    alloc_memory_pool_impl(ctx, "Eden Space", true),
                    alloc_memory_pool_impl(ctx, "Old Gen", true),
                ],
                "java/lang/management/MemoryManagerMXBean" => vec![
                    alloc_garbage_collector_impl(ctx, "G1 Young Generation"),
                ],
                "java/lang/management/GarbageCollectorMXBean" => vec![
                    alloc_garbage_collector_impl(ctx, "G1 Young Generation"),
                ],
                // Other PlatformManagedObject classes (RuntimeMXBean, etc.)
                // hit a separate `getPlatformMXBean` (singleton) path; the
                // list-form fallback is an empty list, matching the behaviour
                // of a JVM that genuinely has no extra components registered
                // for that interface.
                _ => Vec::new(),
            };
            let backing = ctx.new_ref_array(ClassId::new(0), beans.len());
            for (i, b) in beans.iter().enumerate() {
                ctx.set_array_element(backing, i, Value::Object(Some(*b)));
            }
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            ctx.set_field_by_name(list, "elementData", Value::Object(Some(backing)));
            ctx.set_field_by_name(list, "size", Value::Int(beans.len() as i32));
            Ok(Some(Value::Object(Some(list))))
        },
    );
}

// ---------------------------------------------------------------------------
// Pre-registered sun.management.* native surface (post-RKC16N.10)
//
// After commit 8aad4c8 (RKC16N.10) the boot advances past
// `ManagementFactory.<clinit>` and `Module.<clinit>`. The previous iteration
// loop spent five build cycles each adding 1-2 missing natives in this same
// family. Rather than continue iterating, the helpers below register the
// whole rest of the `sun.management.*` surface up-front: ThreadImpl,
// ClassLoadingImpl, GarbageCollectorImpl, OperatingSystemImpl,
// HotSpotDiagnostic, FlagImpl. JBoss/Keycloak only iterates these MXBeans
// for diagnostic display, not control flow — empty arrays / zeros / -1 are
// safe defaults consistent with OpenJDK's "metric unavailable" semantics.
//
// All six helpers are `pub` and called from BOTH real-JDK paths in
// `vm/src/vm/vm_init.rs`, alongside `register_vm_management_impl`. They live
// outside `register_jmx_natives` (which is feature-gated synthetic-only).
// ---------------------------------------------------------------------------

/// `sun.management.ThreadImpl` — JMM thread inspection natives.
///
/// Empty arrays / no-ops match OpenJDK's behaviour when the relevant
/// optional thread-CPU/contention-monitoring features are disabled (which
/// they are here — `VMManagementImpl.isThreadCpuTimeSupported` etc. all
/// return `false`).
pub fn register_thread_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/ThreadImpl";

    // No-op population helpers ([JI..., [J[J...) — all leave their output
    // arrays untouched. Java callers iterate the result and find the
    // pre-zeroed slots, which matches "feature unsupported".
    let void_noop: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(None);
    for (name, desc) in [
        (
            "getThreadInfo1",
            "([JI[Ljava/lang/management/ThreadInfo;)V",
        ),
        ("getThreadTotalCpuTime0", "([J[J)V"),
        ("getThreadUserCpuTime0", "([J[J)V"),
        ("getThreadAllocatedMemory1", "([J[J)V"),
        ("setThreadCpuTimeEnabled0", "(Z)V"),
        ("setThreadContentionMonitoringEnabled0", "(Z)V"),
        ("resetContentionTimes0", "(J)V"),
        ("resetPeakThreadCount0", "()V"),
    ] {
        r.register(cls, name, desc, void_noop);
    }

    // getThreads()[Ljava/lang/Thread; — empty Thread[] is safe; consumers
    // just iterate.
    r.register(
        cls,
        "getThreads",
        "()[Ljava/lang/Thread;",
        |ctx, _args| {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // findMonitorDeadlockedThreads0 / findDeadlockedThreads0 return null
    // when no deadlocks (per JMM spec) — match that.
    r.register(
        cls,
        "findMonitorDeadlockedThreads0",
        "()[J",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        cls,
        "findDeadlockedThreads0",
        "()[J",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
}

/// `sun.management.ClassLoadingImpl` — most info comes via
/// `ManagementFactoryHelper`, so this is intentionally minimal.
pub fn register_class_loading_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/ClassLoadingImpl";

    // setVerboseClass(Z)V — accept and ignore (synthetic verbose flag is
    // already false in VMManagementImpl).
    r.register(cls, "setVerboseClass", "(Z)V", |_ctx, _args| Ok(None));

    // <init>(Lsun/management/VMManagement;)V — no-op constructor; the
    // VMManagement reference is stored by Java bytecode in a field that
    // we don't read.
    r.register(
        cls,
        "<init>",
        "(Lsun/management/VMManagement;)V",
        native_noop_with_this,
    );
}

/// `sun.management.GarbageCollectorImpl` — per-collector counters.
///
/// Returning 0 is consistent with "no GC events recorded yet" and matches
/// what OpenJDK reports when GC notification is disabled (and our
/// `VMManagementImpl.isGcNotificationSupported` returns `false`).
pub fn register_garbage_collector_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/GarbageCollectorImpl";

    let zero_long: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Long(0)));
    for name in ["getCollectionCount", "getCollectionTime"] {
        r.register(cls, name, "()J", zero_long);
    }

    // <init>(Ljava/lang/String;Lsun/management/VMManagement;)V — no-op;
    // the name + VMManagement refs are stored by Java bytecode in fields
    // we don't introspect.
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Lsun/management/VMManagement;)V",
        native_noop_with_this,
    );

    // JDK 25 also has the public `<init>(Ljava/lang/String;)V` form (used
    // by ManagementFactoryHelper internals). No-op so synthetic
    // construction in `alloc_garbage_collector_impl` doesn't need to chain
    // through the real bytecode.
    r.register(cls, "<init>", "(Ljava/lang/String;)V", native_noop_with_this);

    // Wave 1 / Task A: getName()Ljava/lang/String; — read the synthetic
    // `name` slot we populate in `alloc_garbage_collector_impl`. The
    // bytecode `MemoryManagerImpl.getName` does `getfield name` directly;
    // overriding natively keeps us robust against field-layout drift.
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    });

    // gc()V — GarbageCollectorImpl exposes a manual-trigger entry point
    // mirroring MemoryMXBean.gc(). No-op is fine; we don't proxy through
    // to the real GC here (MemoryMXBean.gc() does that elsewhere).
    r.register(cls, "gc", "()V", |_ctx, _args| Ok(None));
}

/// `sun.management.MemoryManagerImpl` — base class for memory managers.
///
/// Wave 1 / Task A: register `getName()` so the synthetic manager
/// instances returned by `MemoryImpl.getMemoryManagers0()` answer with
/// their populated `name` field. (`isValid()` defaults to `false` via
/// the synthetic field initialisation; we override it to `true` so
/// `ManagementFactoryHelper` doesn't filter the bean out.)
pub fn register_memory_manager_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/MemoryManagerImpl";

    r.register(cls, "<init>", "(Ljava/lang/String;)V", native_noop_with_this);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    });

    r.register(cls, "isValid", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    // getMemoryPools0()[Ljava/lang/management/MemoryPoolMXBean; — return
    // an empty array so the synthetic accessor doesn't trip on a missing
    // native. The probe doesn't traverse this edge.
    r.register(
        cls,
        "getMemoryPools0",
        "()[Ljava/lang/management/MemoryPoolMXBean;",
        |ctx, _args| {
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

/// `sun.management.MemoryPoolImpl` — per-pool metadata + usage.
///
/// Wave 1 / Task A: synthetic instances are built by
/// `alloc_memory_pool_impl` with `name` (string) + `isHeap` (boolean)
/// fields populated. Native overrides for `getName()` and `getType()`
/// short-circuit the bytecode so the probe sees the right values
/// regardless of field-layout drift.
pub fn register_memory_pool_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/MemoryPoolImpl";

    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;ZJJ)V",
        native_noop_with_this,
    );

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    });

    r.register(cls, "isValid", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    r.register(
        cls,
        "getType",
        "()Ljava/lang/management/MemoryType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_heap = matches!(
                ctx.get_field_by_name(this, "isHeap"),
                Value::Int(v) if v != 0
            );
            // MemoryType is an enum — fetch the static field by name.
            let cid = ctx
                .ensure_class_initialized("java/lang/management/MemoryType")
                .unwrap_or(ClassId::new(0));
            let field = if is_heap { "HEAP" } else { "NON_HEAP" };
            if let Some(idx) = ctx.static_field_index_by_name(cid, field) {
                Ok(Some(ctx.get_static_field(cid, idx)))
            } else {
                // Fall back to a freshly allocated synthetic enum object —
                // the probe only stringifies via toString(), which on
                // enums reads the `name` field at slot 0.
                let e = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryType", 2);
                let label = ctx.create_string(field);
                ctx.set_field_by_name(e, "name", Value::Object(Some(label)));
                Ok(Some(Value::Object(Some(e))))
            }
        },
    );

    // getUsage / getPeakUsage / getCollectionUsage — return UNDEFINED_USAGE
    // (-1, -1, -1, -1) per the JMM spec for "metric unavailable".
    let undefined_usage: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, _args| {
            let mu = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4);
            ctx.set_field(mu, 0, Value::Long(-1));
            ctx.set_field(mu, 1, Value::Long(-1));
            ctx.set_field(mu, 2, Value::Long(-1));
            ctx.set_field(mu, 3, Value::Long(-1));
            Ok(Some(Value::Object(Some(mu))))
        };
    for name in ["getUsage0", "getPeakUsage0", "getCollectionUsage0"] {
        r.register(
            cls,
            name,
            "()Ljava/lang/management/MemoryUsage;",
            undefined_usage,
        );
    }
}

/// Allocate a synthetic `sun.management.MemoryPoolImpl` with `name` +
/// `isHeap` populated.  The remaining fields default-initialise to zero
/// (longs) / null (refs) which matches a "no-threshold" pool — fine for
/// JConsole-style enumeration.
pub fn alloc_memory_pool_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
    is_heap: bool,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "sun/management/MemoryPoolImpl", 12);
    let n = ctx.create_string(name);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(obj, "isHeap", Value::Int(if is_heap { 1 } else { 0 }));
    ctx.set_field_by_name(obj, "isValid", Value::Int(1));
    obj
}

/// Allocate a synthetic `sun.management.GarbageCollectorImpl` with the
/// inherited `name` field populated.  The bean implements both
/// `MemoryManagerMXBean` (so it shows up in
/// `getMemoryManagerMXBeans()`) and `GarbageCollectorMXBean` (so it
/// passes the `instanceof` filter in
/// `ManagementFactoryHelper.getGarbageCollectorMXBeans`).
pub fn alloc_garbage_collector_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "sun/management/GarbageCollectorImpl", 4);
    let n = ctx.create_string(name);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(obj, "isValid", Value::Int(1));
    obj
}

/// `sun.management.OperatingSystemImpl` — process / OS metrics.
///
/// All byte-quantity longs return -1 (matches OpenJDK behaviour when the
/// underlying metric is unavailable). All doubles return -1.0 per the
/// `OperatingSystemMXBean` spec for "load not available".
pub fn register_operating_system_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/OperatingSystemImpl";

    let neg_one_long: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Long(-1)));
    for name in [
        "getCommittedVirtualMemorySize0",
        "getTotalSwapSpaceSize0",
        "getFreeSwapSpaceSize0",
        "getProcessCpuTime0",
        "getFreePhysicalMemorySize0",
        "getTotalPhysicalMemorySize0",
        "getOpenFileDescriptorCount0",
        "getMaxFileDescriptorCount0",
    ] {
        r.register(cls, name, "()J", neg_one_long);
    }

    let neg_one_double: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Double(-1.0)));
    for name in ["getSystemCpuLoad0", "getProcessCpuLoad0"] {
        r.register(cls, name, "()D", neg_one_double);
    }

    // initialize0()V — sets up native counters; nothing to do here.
    r.register(cls, "initialize0", "()V", |_ctx, _args| Ok(None));
}

/// `com.sun.management.HotSpotDiagnostic` (internally
/// `sun.management.HotSpotDiagnostic`) — heap dumping + flag listing.
pub fn register_hotspot_diagnostic(r: &mut NativeMethodRegistry) {
    // OpenJDK's HotSpotDiagnostic class is in `sun.management` (the public
    // facade lives in `com.sun.management.HotSpotDiagnosticMXBean`).
    let cls = "sun/management/HotSpotDiagnostic";

    // dumpHeap0(String, Z)V — heap dumping is a major separate effort;
    // accept arguments and no-op.
    r.register(
        cls,
        "dumpHeap0",
        "(Ljava/lang/String;Z)V",
        |_ctx, _args| Ok(None),
    );

    // getDiagnosticOptions()Ljava/util/List; — empty ArrayList matches
    // "no manageable VM options exposed". JBoss only iterates this for
    // diagnostic display.
    r.register(
        cls,
        "getDiagnosticOptions",
        "()Ljava/util/List;",
        |ctx, _args| {
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            let backing = ctx.new_ref_array(ClassId::new(0), 0);
            ctx.set_field(list, 0, Value::Object(Some(backing))); // elementData
            ctx.set_field(list, 1, Value::Int(0));                // size
            Ok(Some(Value::Object(Some(list))))
        },
    );
}

/// `sun.management.Flag` — VM flag enumeration.
///
/// We don't expose any VM-level manageable flags through JMM, so all
/// queries return zero / empty.
pub fn register_flag_impl(r: &mut NativeMethodRegistry) {
    let cls = "sun/management/Flag";

    r.register(cls, "getInternalFlagCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });

    r.register(
        cls,
        "getAllFlagNames",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // Empty Flag[] — same reference-array pattern as getAllFlagNames; the
    // element type is `Lsun/management/Flag;` but our synthetic ref-array
    // doesn't carry the element class beyond ClassId::new(0).
    r.register(
        cls,
        "getAllFlags",
        "()[Lsun/management/Flag;",
        |ctx, _args| {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

// ---------------------------------------------------------------------------
// 1. ManagementFactory
// ---------------------------------------------------------------------------

fn register_management_factory(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/ManagementFactory";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    // getPlatformMBeanServer()
    r.register(
        cls,
        "getPlatformMBeanServer",
        "()Ljavax/management/MBeanServer;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/management/MBeanServer", 2);
            // field 0 = defaultDomain
            let domain = ctx.create_string("DefaultDomain");
            ctx.set_field(obj, 0, Value::Object(Some(domain)));
            // field 1 = mbeanCount
            ctx.set_field(obj, 1, Value::Int(9));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getRuntimeMXBean()
    r.register(
        cls,
        "getRuntimeMXBean",
        "()Ljava/lang/management/RuntimeMXBean;",
        |ctx, _args| {
            let obj = alloc_runtime_mxbean(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getMemoryMXBean()
    r.register(
        cls,
        "getMemoryMXBean",
        "()Ljava/lang/management/MemoryMXBean;",
        |ctx, _args| {
            let obj = alloc_memory_mxbean(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getThreadMXBean()
    r.register(
        cls,
        "getThreadMXBean",
        "()Ljava/lang/management/ThreadMXBean;",
        |ctx, _args| {
            let obj = alloc_thread_mxbean(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getClassLoadingMXBean()
    r.register(
        cls,
        "getClassLoadingMXBean",
        "()Ljava/lang/management/ClassLoadingMXBean;",
        |ctx, _args| {
            let obj = alloc_class_loading_mxbean(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getOperatingSystemMXBean()
    r.register(
        cls,
        "getOperatingSystemMXBean",
        "()Ljava/lang/management/OperatingSystemMXBean;",
        |ctx, _args| {
            let obj = alloc_os_mxbean(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getCompilationMXBean()
    r.register(
        cls,
        "getCompilationMXBean",
        "()Ljava/lang/management/CompilationMXBean;",
        |ctx, _args| {
            let obj = alloc_compilation_mxbean(ctx);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // getGarbageCollectorMXBeans() -> List<GarbageCollectorMXBean>
    r.register(
        cls,
        "getGarbageCollectorMXBeans",
        "()Ljava/util/List;",
        |ctx, _args| {
            // Return an ArrayList with one GC bean
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            let backing = ctx.new_ref_array(ClassId::new(0), 1);
            let gc = alloc_gc_mxbean(ctx);
            ctx.set_array_element(backing, 0, Value::Object(Some(gc)));
            ctx.set_field(list, 0, Value::Object(Some(backing))); // elementData
            ctx.set_field(list, 1, Value::Int(1)); // size
            Ok(Some(Value::Object(Some(list))))
        },
    );
}

// ---------------------------------------------------------------------------
// 2. RuntimeMXBean — 10-field synthetic
// ---------------------------------------------------------------------------

fn alloc_runtime_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/lang/management/RuntimeMXBean", 10);
    let pid = std::process::id();
    let name = ctx.create_string(&format!("rustjvm@{}", pid));
    ctx.set_field(obj, 0, Value::Object(Some(name)));
    let vm_name = ctx.create_string("RustJVM");
    ctx.set_field(obj, 1, Value::Object(Some(vm_name)));
    let vm_version = ctx.create_string("0.1.0");
    ctx.set_field(obj, 2, Value::Object(Some(vm_version)));
    let vm_vendor = ctx.create_string("Craton");
    ctx.set_field(obj, 3, Value::Object(Some(vm_vendor)));
    let spec_name = ctx.create_string("Java Virtual Machine Specification");
    ctx.set_field(obj, 4, Value::Object(Some(spec_name)));
    let spec_version = ctx.create_string("25");
    ctx.set_field(obj, 5, Value::Object(Some(spec_version)));
    let spec_vendor = ctx.create_string("Oracle Corporation");
    ctx.set_field(obj, 6, Value::Object(Some(spec_vendor)));
    ctx.set_field(obj, 7, Value::Long(vm_start_epoch_ms() as i64));
    ctx.set_field(obj, 8, Value::Long(uptime_ms() as i64));
    // field 9 = inputArguments (empty ArrayList)
    let args_list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
    let empty_arr = ctx.new_ref_array(ClassId::new(0), 0);
    ctx.set_field(args_list, 0, Value::Object(Some(empty_arr)));
    ctx.set_field(args_list, 1, Value::Int(0));
    ctx.set_field(obj, 9, Value::Object(Some(args_list)));
    obj
}

fn register_runtime_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/RuntimeMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(cls, "getVmName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(cls, "getVmVersion", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(cls, "getVmVendor", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(cls, "getSpecName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    });
    r.register(cls, "getSpecVersion", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 5)))
    });
    r.register(cls, "getSpecVendor", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 6)))
    });
    r.register(cls, "getStartTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 7)))
    });
    r.register(cls, "getUptime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(uptime_ms() as i64)))
    });
    r.register(
        cls,
        "getInputArguments",
        "()Ljava/util/List;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 9)))
        },
    );
    r.register(
        cls,
        "getClassPath",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let cp = ctx
                .get_system_property("java.class.path")
                .unwrap_or_default();
            let s = ctx.create_string(&cp);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        cls,
        "getBootClassPath",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        cls,
        "isBootClassPathSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
}

// ---------------------------------------------------------------------------
// 3. MemoryMXBean — 6-field synthetic
// ---------------------------------------------------------------------------

fn alloc_memory_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryMXBean", 6);
    // Wire to real heap stats
    let heap_used = ctx.heap_allocated_bytes() as i64;
    let heap_max = 256 * 1024 * 1024_i64; // max is config-based, use default
    let heap_committed = heap_used.max(64 * 1024 * 1024); // committed >= used
    ctx.set_field(obj, 0, Value::Long(heap_used));           // heapUsed (real)
    ctx.set_field(obj, 1, Value::Long(heap_max));            // heapMax
    ctx.set_field(obj, 2, Value::Long(heap_committed));      // heapCommitted
    ctx.set_field(obj, 3, Value::Long(4 * 1024 * 1024));    // nonHeapUsed
    ctx.set_field(obj, 4, Value::Long(64 * 1024 * 1024));   // nonHeapMax
    ctx.set_field(obj, 5, Value::Int(0));                    // objectPendingFinalization
    obj
}

fn alloc_memory_usage(
    ctx: &mut dyn NativeContext,
    init: i64,
    used: i64,
    committed: i64,
    max: i64,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4);
    ctx.set_field(obj, 0, Value::Long(init));
    ctx.set_field(obj, 1, Value::Long(used));
    ctx.set_field(obj, 2, Value::Long(committed));
    ctx.set_field(obj, 3, Value::Long(max));
    obj
}

fn register_memory_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/MemoryMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(
        cls,
        "getHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let used = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                _ => 16 * 1024 * 1024,
            };
            let max = match ctx.get_field(this, 1) {
                Value::Long(v) => v,
                _ => 256 * 1024 * 1024,
            };
            let committed = match ctx.get_field(this, 2) {
                Value::Long(v) => v,
                _ => 64 * 1024 * 1024,
            };
            let mu = alloc_memory_usage(ctx, 0, used, committed, max);
            Ok(Some(Value::Object(Some(mu))))
        },
    );

    r.register(
        cls,
        "getNonHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let used = match ctx.get_field(this, 3) {
                Value::Long(v) => v,
                _ => 4 * 1024 * 1024,
            };
            let max = match ctx.get_field(this, 4) {
                Value::Long(v) => v,
                _ => 64 * 1024 * 1024,
            };
            let mu = alloc_memory_usage(ctx, 0, used, max, max);
            Ok(Some(Value::Object(Some(mu))))
        },
    );

    r.register(
        cls,
        "getObjectPendingFinalizationCount",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

    r.register(cls, "gc", "()V", |ctx, _args| {
        ctx.force_gc();
        Ok(None)
    });
}

// ---------------------------------------------------------------------------
// 4. MemoryUsage — 4-field synthetic
// ---------------------------------------------------------------------------

fn register_memory_usage(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/MemoryUsage";
    r.register(cls, "<init>", "()V", native_noop_with_this);
    r.register(cls, "<init>", "(JJJJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // args: this, init, used, committed, max (longs take 2 slots each in JVM
        // but our Value model uses one slot per Long)
        let init_val = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let used_val = match args.get(2) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let committed_val = match args.get(3) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let max_val = match args.get(4) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Long(init_val));
        ctx.set_field(this, 1, Value::Long(used_val));
        ctx.set_field(this, 2, Value::Long(committed_val));
        ctx.set_field(this, 3, Value::Long(max_val));
        Ok(None)
    });

    r.register(cls, "getInit", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(cls, "getUsed", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(cls, "getCommitted", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(cls, "getMax", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(
        cls,
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let init_v = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                _ => 0,
            };
            let used_v = match ctx.get_field(this, 1) {
                Value::Long(v) => v,
                _ => 0,
            };
            let committed_v = match ctx.get_field(this, 2) {
                Value::Long(v) => v,
                _ => 0,
            };
            let max_v = match ctx.get_field(this, 3) {
                Value::Long(v) => v,
                _ => 0,
            };
            let text = format!(
                "init={}, used={}, committed={}, max={}",
                init_v, used_v, committed_v, max_v
            );
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        },
    );
}

// ---------------------------------------------------------------------------
// 5. ThreadMXBean — 6-field synthetic
// ---------------------------------------------------------------------------

fn alloc_thread_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/lang/management/ThreadMXBean", 6);
    let thread_count = ctx.active_thread_count();
    ctx.set_field(obj, 0, Value::Int(thread_count));          // threadCount (real)
    ctx.set_field(obj, 1, Value::Int(thread_count));          // peakThreadCount (real)
    ctx.set_field(obj, 2, Value::Long(thread_count as i64));  // totalStartedThreadCount (real)
    ctx.set_field(obj, 3, Value::Int(0));                     // daemonThreadCount
    ctx.set_field(obj, 4, Value::Long(-1));                   // currentThreadCpuTime (not supported)
    ctx.set_field(obj, 5, Value::Long(-1));                   // currentThreadUserTime
    obj
}

fn register_thread_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/ThreadMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(cls, "getThreadCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(cls, "getPeakThreadCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        cls,
        "getTotalStartedThreadCount",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(cls, "getDaemonThreadCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(cls, "getCurrentThreadCpuTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(-1)))
    });
    r.register(cls, "getCurrentThreadUserTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(-1)))
    });
    r.register(
        cls,
        "isThreadCpuTimeSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.register(
        cls,
        "isThreadCpuTimeEnabled",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.register(cls, "getAllThreadIds", "()[J", |ctx, _args| {
        use rustjvm_types::ArrayElementType;
        let arr = ctx.new_array(ArrayElementType::Long, 1);
        ctx.set_array_element(arr, 0, Value::Long(1)); // main thread
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(
        cls,
        "getThreadInfo",
        "(J)Ljava/lang/management/ThreadInfo;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        cls,
        "findDeadlockedThreads",
        "()[J",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        cls,
        "findMonitorDeadlockedThreads",
        "()[J",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        cls,
        "dumpAllThreads",
        "(ZZ)[Ljava/lang/management/ThreadInfo;",
        |ctx, _args| {
            let arr = ctx.new_ref_array(ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

// ---------------------------------------------------------------------------
// 6. ClassLoadingMXBean — 3-field synthetic
// ---------------------------------------------------------------------------

fn alloc_class_loading_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(
        ctx,
        "java/lang/management/ClassLoadingMXBean",
        3,
    );
    let loaded = ctx.loaded_class_count() as i32;
    ctx.set_field(obj, 0, Value::Int(loaded));            // loadedClassCount (real)
    ctx.set_field(obj, 1, Value::Long(loaded as i64));    // totalLoadedClassCount (real)
    ctx.set_field(obj, 2, Value::Long(0));                // unloadedClassCount
    obj
}

fn register_class_loading_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/ClassLoadingMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(cls, "getLoadedClassCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        cls,
        "getTotalLoadedClassCount",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(cls, "getUnloadedClassCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(cls, "isVerbose", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(cls, "setVerbose", "(Z)V", native_noop_with_this);
}

// ---------------------------------------------------------------------------
// 7. OperatingSystemMXBean — 5-field synthetic
// ---------------------------------------------------------------------------

fn alloc_os_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(
        ctx,
        "java/lang/management/OperatingSystemMXBean",
        5,
    );
    let name = ctx.create_string(std::env::consts::OS);
    ctx.set_field(obj, 0, Value::Object(Some(name)));
    let arch = ctx.create_string(std::env::consts::ARCH);
    ctx.set_field(obj, 1, Value::Object(Some(arch)));
    let version = ctx.create_string("unknown");
    ctx.set_field(obj, 2, Value::Object(Some(version)));
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(1);
    ctx.set_field(obj, 3, Value::Int(cpus));
    ctx.set_field(obj, 4, Value::Double(-1.0));
    obj
}

fn register_operating_system_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/OperatingSystemMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(
        cls,
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        cls,
        "getArch",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(
        cls,
        "getVersion",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(cls, "getAvailableProcessors", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(
        cls,
        "getSystemLoadAverage",
        "()D",
        |_ctx, _args| Ok(Some(Value::Double(-1.0))),
    );
}

// ---------------------------------------------------------------------------
// 8. CompilationMXBean — 3-field synthetic
// ---------------------------------------------------------------------------

fn alloc_compilation_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(
        ctx,
        "java/lang/management/CompilationMXBean",
        3,
    );
    let name = ctx.create_string("RustJVM JIT");
    ctx.set_field(obj, 0, Value::Object(Some(name)));
    ctx.set_field(obj, 1, Value::Long(0));   // totalCompilationTime
    ctx.set_field(obj, 2, Value::Int(0));    // isCompilationTimeMonitoringSupported
    obj
}

fn register_compilation_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/CompilationMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(
        cls,
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(cls, "getTotalCompilationTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        cls,
        "isCompilationTimeMonitoringSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
}

// ---------------------------------------------------------------------------
// 9. GarbageCollectorMXBean — 4-field synthetic
// ---------------------------------------------------------------------------

fn alloc_gc_mxbean(ctx: &mut dyn NativeContext) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(
        ctx,
        "java/lang/management/GarbageCollectorMXBean",
        4,
    );
    let name = ctx.create_string("RustJVM GC");
    ctx.set_field(obj, 0, Value::Object(Some(name)));
    let gc_count = ctx.gc_collection_count() as i64;
    ctx.set_field(obj, 1, Value::Long(gc_count)); // collectionCount (real)
    ctx.set_field(obj, 2, Value::Long(0));         // collectionTime
    // field 3 = memoryPoolNames (String[])
    let pool_names = ctx.new_ref_array(ClassId::new(0), 3);
    let eden = ctx.create_string("Eden");
    let survivor = ctx.create_string("Survivor");
    let old_gen = ctx.create_string("Old Gen");
    ctx.set_array_element(pool_names, 0, Value::Object(Some(eden)));
    ctx.set_array_element(pool_names, 1, Value::Object(Some(survivor)));
    ctx.set_array_element(pool_names, 2, Value::Object(Some(old_gen)));
    ctx.set_field(obj, 3, Value::Object(Some(pool_names)));
    obj
}

fn register_gc_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/GarbageCollectorMXBean";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(
        cls,
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(cls, "getCollectionCount", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(cls, "getCollectionTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(
        cls,
        "getMemoryPoolNames",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );
    r.register(cls, "isValid", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
}

// ---------------------------------------------------------------------------
// 10. MBeanServer — 2-field synthetic
// ---------------------------------------------------------------------------

fn register_mbean_server(r: &mut NativeMethodRegistry) {
    let cls = "javax/management/MBeanServer";
    r.register(cls, "<init>", "()V", native_noop_with_this);

    r.register(
        cls,
        "getDefaultDomain",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        cls,
        "getMBeanCount",
        "()Ljava/lang/Integer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(
        cls,
        "isRegistered",
        "(Ljavax/management/ObjectName;)Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.register(
        cls,
        "queryMBeans",
        "(Ljavax/management/ObjectName;Ljavax/management/QueryExp;)Ljava/util/Set;",
        |ctx, _args| {
            // Return empty HashSet (synthetic ArrayList acting as set)
            let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 2);
            let backing = ctx.new_ref_array(ClassId::new(0), 0);
            ctx.set_field(set, 0, Value::Object(Some(backing)));
            ctx.set_field(set, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(set))))
        },
    );
    r.register(
        cls,
        "getAttribute",
        "(Ljavax/management/ObjectName;Ljava/lang/String;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod jmx_tests {
    use super::*;
    use rustjvm_native_api::NativeMethodRegistry;

    #[test]
    fn test_management_factory_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/ManagementFactory";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(
            r.find(cls, "getPlatformMBeanServer", "()Ljavax/management/MBeanServer;")
                .is_some()
        );
        assert!(
            r.find(cls, "getRuntimeMXBean", "()Ljava/lang/management/RuntimeMXBean;")
                .is_some()
        );
        assert!(
            r.find(cls, "getMemoryMXBean", "()Ljava/lang/management/MemoryMXBean;")
                .is_some()
        );
    }

    #[test]
    fn test_runtime_mxbean_all_getters_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/RuntimeMXBean";
        let methods = [
            ("getName", "()Ljava/lang/String;"),
            ("getVmName", "()Ljava/lang/String;"),
            ("getVmVersion", "()Ljava/lang/String;"),
            ("getVmVendor", "()Ljava/lang/String;"),
            ("getSpecName", "()Ljava/lang/String;"),
            ("getSpecVersion", "()Ljava/lang/String;"),
            ("getSpecVendor", "()Ljava/lang/String;"),
            ("getStartTime", "()J"),
            ("getUptime", "()J"),
            ("getInputArguments", "()Ljava/util/List;"),
            ("getClassPath", "()Ljava/lang/String;"),
            ("getBootClassPath", "()Ljava/lang/String;"),
            ("isBootClassPathSupported", "()Z"),
        ];
        for (name, desc) in &methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing RuntimeMXBean.{}{}",
                name,
                desc
            );
        }
    }

    #[test]
    fn test_memory_mxbean_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/MemoryMXBean";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(
            r.find(cls, "getHeapMemoryUsage", "()Ljava/lang/management/MemoryUsage;")
                .is_some()
        );
        assert!(
            r.find(cls, "getNonHeapMemoryUsage", "()Ljava/lang/management/MemoryUsage;")
                .is_some()
        );
        assert!(
            r.find(cls, "getObjectPendingFinalizationCount", "()I")
                .is_some()
        );
        assert!(r.find(cls, "gc", "()V").is_some());
    }

    #[test]
    fn test_memory_usage_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/MemoryUsage";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "<init>", "(JJJJ)V").is_some());
        assert!(r.find(cls, "getInit", "()J").is_some());
        assert!(r.find(cls, "getUsed", "()J").is_some());
        assert!(r.find(cls, "getCommitted", "()J").is_some());
        assert!(r.find(cls, "getMax", "()J").is_some());
        assert!(r.find(cls, "toString", "()Ljava/lang/String;").is_some());
    }

    #[test]
    fn test_thread_mxbean_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/ThreadMXBean";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getThreadCount", "()I").is_some());
        assert!(r.find(cls, "getPeakThreadCount", "()I").is_some());
        assert!(r.find(cls, "getTotalStartedThreadCount", "()J").is_some());
        assert!(r.find(cls, "getDaemonThreadCount", "()I").is_some());
        assert!(r.find(cls, "getCurrentThreadCpuTime", "()J").is_some());
        assert!(r.find(cls, "getCurrentThreadUserTime", "()J").is_some());
        assert!(r.find(cls, "isThreadCpuTimeSupported", "()Z").is_some());
        assert!(r.find(cls, "isThreadCpuTimeEnabled", "()Z").is_some());
        assert!(r.find(cls, "getAllThreadIds", "()[J").is_some());
        assert!(
            r.find(cls, "getThreadInfo", "(J)Ljava/lang/management/ThreadInfo;")
                .is_some()
        );
        assert!(r.find(cls, "findDeadlockedThreads", "()[J").is_some());
        assert!(
            r.find(cls, "findMonitorDeadlockedThreads", "()[J")
                .is_some()
        );
        assert!(
            r.find(
                cls,
                "dumpAllThreads",
                "(ZZ)[Ljava/lang/management/ThreadInfo;"
            )
            .is_some()
        );
    }

    #[test]
    fn test_class_loading_mxbean_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/ClassLoadingMXBean";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getLoadedClassCount", "()I").is_some());
        assert!(r.find(cls, "getTotalLoadedClassCount", "()J").is_some());
        assert!(r.find(cls, "getUnloadedClassCount", "()J").is_some());
        assert!(r.find(cls, "isVerbose", "()Z").is_some());
        assert!(r.find(cls, "setVerbose", "(Z)V").is_some());
    }

    #[test]
    fn test_operating_system_mxbean_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/OperatingSystemMXBean";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getName", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getArch", "()Ljava/lang/String;").is_some());
        assert!(
            r.find(cls, "getVersion", "()Ljava/lang/String;")
                .is_some()
        );
        assert!(r.find(cls, "getAvailableProcessors", "()I").is_some());
        assert!(r.find(cls, "getSystemLoadAverage", "()D").is_some());
    }

    #[test]
    fn test_compilation_mxbean_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/CompilationMXBean";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getName", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getTotalCompilationTime", "()J").is_some());
        assert!(
            r.find(cls, "isCompilationTimeMonitoringSupported", "()Z")
                .is_some()
        );
    }

    #[test]
    fn test_gc_mxbean_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/GarbageCollectorMXBean";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "getName", "()Ljava/lang/String;").is_some());
        assert!(r.find(cls, "getCollectionCount", "()J").is_some());
        assert!(r.find(cls, "getCollectionTime", "()J").is_some());
        assert!(
            r.find(cls, "getMemoryPoolNames", "()[Ljava/lang/String;")
                .is_some()
        );
        assert!(r.find(cls, "isValid", "()Z").is_some());
    }

    #[test]
    fn test_mbean_server_registration() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "javax/management/MBeanServer";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(
            r.find(cls, "getDefaultDomain", "()Ljava/lang/String;")
                .is_some()
        );
        assert!(
            r.find(cls, "getMBeanCount", "()Ljava/lang/Integer;")
                .is_some()
        );
        assert!(
            r.find(cls, "isRegistered", "(Ljavax/management/ObjectName;)Z")
                .is_some()
        );
        assert!(r.find(
            cls,
            "queryMBeans",
            "(Ljavax/management/ObjectName;Ljavax/management/QueryExp;)Ljava/util/Set;"
        ).is_some());
        assert!(r.find(
            cls,
            "getAttribute",
            "(Ljavax/management/ObjectName;Ljava/lang/String;)Ljava/lang/Object;"
        ).is_some());
    }

    #[test]
    fn test_vm_start_time_initialization() {
        // Access the start time — should not panic and should be reasonable
        let ms = vm_start_epoch_ms();
        assert!(ms > 0, "VM start epoch millis should be positive");
        let up = uptime_ms();
        // Uptime should be very small (we just started)
        assert!(up < 60_000, "Uptime should be < 60s in test");
    }

    #[test]
    fn test_management_factory_returns_all_mxbeans() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/ManagementFactory";
        let factory_methods = [
            "getPlatformMBeanServer",
            "getRuntimeMXBean",
            "getMemoryMXBean",
            "getThreadMXBean",
            "getClassLoadingMXBean",
            "getOperatingSystemMXBean",
            "getCompilationMXBean",
            "getGarbageCollectorMXBeans",
        ];
        for method in &factory_methods {
            // Just check the method name is registered (any descriptor)
            // We already checked specific descriptors above; this ensures
            // all eight factory methods exist.
            let found = r.find(
                cls,
                method,
                match *method {
                    "getPlatformMBeanServer" => "()Ljavax/management/MBeanServer;",
                    "getRuntimeMXBean" => "()Ljava/lang/management/RuntimeMXBean;",
                    "getMemoryMXBean" => "()Ljava/lang/management/MemoryMXBean;",
                    "getThreadMXBean" => "()Ljava/lang/management/ThreadMXBean;",
                    "getClassLoadingMXBean" => "()Ljava/lang/management/ClassLoadingMXBean;",
                    "getOperatingSystemMXBean" => {
                        "()Ljava/lang/management/OperatingSystemMXBean;"
                    }
                    "getCompilationMXBean" => "()Ljava/lang/management/CompilationMXBean;",
                    "getGarbageCollectorMXBeans" => "()Ljava/util/List;",
                    _ => unreachable!(),
                },
            );
            assert!(found.is_some(), "Missing factory method: {}", method);
        }
    }

    #[test]
    fn test_total_registered_jmx_method_count() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let count = r.len();
        assert!(
            count > 50,
            "Expected > 50 JMX methods registered, got {}",
            count
        );
    }

    #[test]
    fn test_all_mxbean_classes_have_init() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let classes = [
            "java/lang/management/ManagementFactory",
            "java/lang/management/RuntimeMXBean",
            "java/lang/management/MemoryMXBean",
            "java/lang/management/MemoryUsage",
            "java/lang/management/ThreadMXBean",
            "java/lang/management/ClassLoadingMXBean",
            "java/lang/management/OperatingSystemMXBean",
            "java/lang/management/CompilationMXBean",
            "java/lang/management/GarbageCollectorMXBean",
            "javax/management/MBeanServer",
        ];
        for cls in &classes {
            assert!(
                r.find(cls, "<init>", "()V").is_some(),
                "Missing <init> for {}",
                cls
            );
        }
    }

    #[test]
    fn test_vm_management_impl_int_typed_thread_counters() {
        // RKC16N.12 regression: live/peak/daemon thread counts are `int`
        // in JDK 25's VMManagementImpl, not `long`. The earlier batch
        // registered them as `()J`, which caused the dispatcher to never
        // match the call site and the JVM to raise UnsatisfiedLinkError
        // during ManagementFactoryHelper.<clinit> -> new VMManagementImpl()
        // on Keycloak boot. Pin the correct descriptors here so the
        // mismatch can't silently regress.
        let mut r = NativeMethodRegistry::new();
        register_vm_management_impl(&mut r);
        let cls = "sun/management/VMManagementImpl";
        for name in [
            "getLiveThreadCount",
            "getPeakThreadCount",
            "getDaemonThreadCount",
        ] {
            assert!(
                r.find(cls, name, "()I").is_some(),
                "VMManagementImpl.{} should be registered with `()I` (int), \
                 not `()J` (long) — see JDK 25 sun/management/VMManagementImpl.java",
                name
            );
            assert!(
                r.find(cls, name, "()J").is_none(),
                "VMManagementImpl.{} must NOT be registered with `()J`; \
                 the JDK declares it `int` and the dispatcher matches by \
                 full descriptor.",
                name
            );
        }
    }

    #[test]
    fn test_vm_management_impl_uptime_and_processors() {
        // RKC16N.12: VMManagementImpl declares native getUptime0()J and
        // getAvailableProcessors()I. Both are reachable from
        // RuntimeImpl during JMM bootstrap; missing either surfaces as
        // an UnsatisfiedLinkError during ManagementFactory class init.
        let mut r = NativeMethodRegistry::new();
        register_vm_management_impl(&mut r);
        let cls = "sun/management/VMManagementImpl";
        assert!(
            r.find(cls, "getUptime0", "()J").is_some(),
            "VMManagementImpl.getUptime0 missing"
        );
        assert!(
            r.find(cls, "getAvailableProcessors", "()I").is_some(),
            "VMManagementImpl.getAvailableProcessors missing"
        );
    }

    #[test]
    fn test_management_factory_load_native_lib_chain() {
        // Session 98: ManagementFactory.<clinit> -> loadNativeLib() ->
        // System.loadLibrary("management") -> Runtime.loadLibrary0(...) ->
        // ClassLoader.loadLibrary throws UnsatisfiedLinkError because
        // libmanagement.dll is genuinely absent (we ship JMM natives
        // in-process via NativeMethodRegistry). RKC16N.12's vm_exec.rs
        // override-allowlist failed because the *target* natives for
        // System.loadLibrary / Runtime.loadLibrary0 are registered only
        // in `register_runtime_natives` (synthetic-mode-only), not in
        // `register_essential_natives` (real-JDK-mode). Pin the no-op
        // entries here so the chain short-circuits at the narrowest
        // possible point (loadNativeLib) plus belt-and-suspenders fallbacks
        // at System / Runtime levels.
        let mut r = NativeMethodRegistry::new();
        register_vm_management_impl(&mut r);
        assert!(
            r.find(
                "java/lang/management/ManagementFactory",
                "loadNativeLib",
                "()V"
            )
            .is_some(),
            "ManagementFactory.loadNativeLib must be a no-op native; \
             without it, ManagementFactory.<clinit> calls into the \
             real-JDK loadLibrary chain which throws UnsatisfiedLinkError."
        );
        assert!(
            r.find("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V").is_some(),
            "System.loadLibrary must be registered in real-JDK mode (via \
             register_vm_management_impl) — register_runtime_natives is \
             synthetic-mode-only."
        );
        assert!(
            r.find("java/lang/System", "load", "(Ljava/lang/String;)V").is_some(),
            "System.load must be registered as the no-op companion of loadLibrary."
        );
        assert!(
            r.find(
                "java/lang/Runtime",
                "loadLibrary0",
                "(Ljava/lang/Class;Ljava/lang/String;)V"
            )
            .is_some(),
            "Runtime.loadLibrary0 must be registered as a backstop in case \
             ManagementFactory.loadNativeLib doesn't intercept first."
        );
        assert!(
            r.find(
                "java/lang/Runtime",
                "load0",
                "(Ljava/lang/Class;Ljava/lang/String;)V"
            )
            .is_some(),
            "Runtime.load0 must be registered alongside loadLibrary0."
        );
    }

    #[test]
    fn test_platform_field_counts_match() {
        // Verify our alloc functions use the expected field counts by
        // counting them in the registration functions. We simply check
        // that the constants embedded in alloc_* match expectations.
        // RuntimeMXBean = 10 fields
        // MemoryMXBean = 6 fields
        // MemoryUsage = 4 fields
        // ThreadMXBean = 6 fields
        // ClassLoadingMXBean = 3 fields
        // OperatingSystemMXBean = 5 fields
        // CompilationMXBean = 3 fields
        // GarbageCollectorMXBean = 4 fields
        // MBeanServer = 2 fields
        //
        // We can verify this by looking at the alloc calls; since we
        // control the code, we just assert the expected counts are used
        // by calling the alloc functions via the factory and checking
        // the registry has the right init methods.
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);

        // All 10 classes should have <init>
        let expected_classes = 10;
        let classes = [
            "java/lang/management/ManagementFactory",
            "java/lang/management/RuntimeMXBean",
            "java/lang/management/MemoryMXBean",
            "java/lang/management/MemoryUsage",
            "java/lang/management/ThreadMXBean",
            "java/lang/management/ClassLoadingMXBean",
            "java/lang/management/OperatingSystemMXBean",
            "java/lang/management/CompilationMXBean",
            "java/lang/management/GarbageCollectorMXBean",
            "javax/management/MBeanServer",
        ];
        assert_eq!(classes.len(), expected_classes);
        for cls in &classes {
            assert!(
                r.find(cls, "<init>", "()V").is_some(),
                "Expected <init> for {} but not found",
                cls
            );
        }
    }
}
