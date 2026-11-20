#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// Construct a Java `IOException` with the given message — the standard
/// way to surface a connection-style failure to JDK callers.
pub(in crate::jmx) fn jmx_ioex<S: Into<String>>(message: S) -> MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

/// The FALLBACK VM start origin, for a context that does not report its VM's
/// own (`NativeContext::vm_start_instant` answers `None`: the mocks).
/// Initialised once, at the first VM's native registration
/// (`register_vm_management_impl`, gc-common w8-f), or on first access by a
/// path that registered nothing (unit tests). Process-wide: one origin for
/// every such context. A context that knows its VM's start uses
/// [`VM_CLOCKS`] instead (gc-common w17-e).
pub(in crate::jmx) static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Epoch millis corresponding to [`PROCESS_START`].
pub(in crate::jmx) static PROCESS_START_EPOCH_MS: OnceLock<u64> = OnceLock::new();

/// One VM's start origin: the instant the VM reports and the wall-clock
/// millis it corresponds to.
#[derive(Clone, Copy)]
pub(in crate::jmx) struct VmClock {
    pub(in crate::jmx) start: Instant,
    pub(in crate::jmx) start_epoch_ms: u64,
}

/// Each VM's start origin, keyed by VM identity (gc-common w17-e,
/// `common-w16f-jmm-switches-and-start-time-are-process-wide`). The start
/// INSTANT comes from the VM (`NativeContext::vm_start_instant`); what is
/// kept here is its wall-clock twin, derived once on the VM's first query so
/// `getStartTime()` answers the same value every time (re-deriving it from
/// `now - elapsed` on each call would drift with the wall clock). Dropped at
/// teardown in [`forget_vm_platform_mbean_server`]. Keyed by VM identity, not
/// by an `ObjectRef`: not an address-keyed table.
pub(in crate::jmx) static VM_CLOCKS: cratonvm_native_api::vm_scoped::VmScoped<Option<VmClock>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// The platform MBeanServer is a JVM-wide singleton -- one per VM, keyed by
/// `NativeContext::vm_identity`, the way the built-in ClassLoader singletons
/// are (`classloader::gc_scan_loader_singleton_roots`). Keeping it in a native
/// side table requires explicit GC integration below.
///
/// gc-common w8-f: this was ONE `Option<ObjectRef>` for the whole process, so
/// in a process with several VMs (an embedder, the `vm` test binary) the
/// second VM's `ManagementFactory.getPlatformMBeanServer()` returned an object
/// in the FIRST VM's heap -- every registry read and write that followed
/// touched another heap -- and every VM's collector was handed that object as
/// a root. Now each VM creates and sees its own; the `_for_vm` scan and remap
/// below are the VM-scoped halves (`native_roots.rs`'s `classloaders-and-jmx`
/// row switches to them through `handoff-w8f-jmx-root-row-is-vm-scoped`).
///
/// The map is keyed by VM identity, not by an `ObjectRef`, so it is not an
/// address-keyed table (`vm/src/memory/addr_keyed.rs`).
pub(in crate::jmx) fn platform_mbean_server_store(// addr-keyed: not a heap address -- VM identity; the VALUES are scanned + remapped per VM.
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, ObjectRef>> {
    // addr-keyed: the accessor's table (see above).
    static INSTANCE: OnceLock<std::sync::Mutex<std::collections::HashMap<usize, ObjectRef>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Each VM's platform `MemoryMXBean` (`ManagementFactory.getMemoryMXBean()`),
/// created on first use, keyed like [`platform_mbean_server_store`] and
/// rooted / remapped / dropped by the same three functions. gen r5w3/obs7
/// (2026-09-26; `gengc-r5w3-obs7-memory-mxbean-is-a-fresh-object-per-call`).
///
/// The concrete JDK `ManagementFactory.getMemoryMXBean()` bytecode owns the
/// public singleton path. The old synthetic-stub override had zero invocations
/// in a compatible-mode native census and is retired; this per-VM fallback is
/// retained only for synthetic platform-MXBean routes.
pub(in crate::jmx) fn platform_memory_mxbean_store(// addr-keyed: not a heap address -- VM identity; the VALUES are scanned + remapped per VM.
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, ObjectRef>> {
    // addr-keyed: the accessor's table (see above).
    static INSTANCE: OnceLock<std::sync::Mutex<std::collections::HashMap<usize, ObjectRef>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// This VM's platform `MemoryMXBean`, created on first use. Same locking
/// discipline as [`platform_mbean_server`]: the store's lock is never held
/// across the allocation (which can run Java and collect, and the collection's
/// root scan takes this lock); two first callers may both allocate, and the
/// first to publish wins.
///
/// gci i46/hc (`gci-i46hc-compatible-memory-mxbean-is-not-the-mbean-servers-RETIRED-UNFIXED-20261018`):
/// the bean is the JDK's own singleton, `ManagementFactoryHelper
/// .getMemoryMXBean()`, whenever the class library has one
/// ([`jdk_platform_memory_mxbean`]). That is the bean the platform
/// MBeanServer registers as `java.lang:type=Memory` and the one
/// `getPlatformMXBean(MemoryMXBean.class)` answers; HotSpot hands the same
/// object out on every route. A bean minted here instead was a second
/// `MemoryImpl`: a listener added through the MBeanServer sat on the JDK's
/// bean while `MemoryImpl.createNotification` asked this native and found no
/// listener, so every memory threshold notification to it was dropped.
pub(in crate::jmx) fn platform_memory_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if let Some(&bean) = platform_memory_mxbean_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
    {
        return Ok(bean);
    }
    let fresh = match jdk_platform_memory_mxbean(ctx) {
        Some(bean) => bean,
        None => alloc_memory_mxbean(ctx)?,
    };
    let mut store = platform_memory_mxbean_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Ok(*store.entry(vm).or_insert(fresh))
}



/// gci i46/hc: `ManagementFactoryHelper.getMemoryMXBean()` -- the JDK's one
/// `MemoryImpl` (`new MemoryImpl(jvm)`, built and cached under the helper's
/// class lock), when the class library has the helper and the switch is on.
/// `None` (the caller then mints the bean, as before) on a synthetic JDK, with
/// the switch off, or when the call does not answer a `sun.management
/// .MemoryImpl`. The same delegation `jdk_buffer_pools_in_hotspot_order` makes
/// for the buffer pools.
pub(in crate::jmx) fn jdk_platform_memory_mxbean(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if ctx.is_class_synthetic_stub("sun/management/MemoryImpl")
    {
        return None;
    }
    match ctx.invoke(
        "sun/management/ManagementFactoryHelper",
        "getMemoryMXBean",
        "()Ljava/lang/management/MemoryMXBean;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(bean))))
            if ctx
                .class_name_of_id(ctx.class_id_of_object(bean))
                .as_deref()
                == Some("sun/management/MemoryImpl") =>
        {
            Some(bean)
        }
        _ => None,
    }
}



/// zt t26/x21 (page `zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 31): the
/// platform `HotSpotDiagnosticMXBean`, the one HotSpot 25 hands out --
/// `PlatformMBeanProviderImpl.getDiagnosticMXBean()` (`jdk.management`), which
/// builds `com.sun.management.internal.HotSpotDiagnostic` once and caches it
/// under the class lock, so every route answers the same object. Under
/// `--compatible` the registered `getPlatformMXBean` native (retired under
/// `--real-jdk`, `RETIRED_SHADOW_L2_TRIPLES`) is reached from a door that does
/// not yield `ManagementFactory`'s `SyntheticStub` to the JDK bytecode (row
/// `df7_nl_c`: a compiled `main`), and it answered `null` where the
/// interpreted door's JDK body answered the bean (row `x20_mf_c`). `None` on a
/// synthetic JDK, with the switch off, or when no `HotSpotDiagnostic` comes
/// back; a JDK without the provider's accessor gets a fresh bean.
pub(in crate::jmx) fn jdk_hotspot_diagnostic_mxbean(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    const PROVIDER: &str = "com/sun/management/internal/PlatformMBeanProviderImpl";
    if ctx.is_class_synthetic_stub(HOTSPOT_DIAGNOSTIC)
    {
        return None;
    }
    fn is_bean(ctx: &dyn NativeContext, bean: ObjectRef) -> bool {
        ctx.class_name_of_id(ctx.class_id_of_object(bean)).as_deref() == Some(HOTSPOT_DIAGNOSTIC)
    }
    for descriptor in [
        "()Lcom/sun/management/HotSpotDiagnosticMXBean;",
        "()Lcom/sun/management/internal/HotSpotDiagnostic;",
    ] {
        if !ctx.method_exists(PROVIDER, "getDiagnosticMXBean", descriptor) {
            continue;
        }
        if let Ok(Some(Value::Object(Some(bean)))) =
            ctx.invoke(PROVIDER, "getDiagnosticMXBean", descriptor, &[])
        {
            if is_bean(&*ctx, bean) {
                return Some(bean);
            }
        }
        return None;
    }
    match ctx.new_object_initialized(HOTSPOT_DIAGNOSTIC, "()V", &[]) {
        Ok(Some(Value::Object(Some(bean)))) if is_bean(&*ctx, bean) => Some(bean),
        _ => None,
    }
}

pub(in crate::jmx) fn remap_platform_mbean_server(server: &mut ObjectRef, pointer_map: &cratonvm_types::PointerMap) {
    if let Some(&new_addr) = pointer_map.get(&(server.as_ptr() as usize)) {
        *server = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
    }
}

/// THIS VM's platform MBeanServer as a root (gc-common w8-f): the collecting
/// VM's collector must never be handed another heap's object.
pub fn gc_scan_platform_mbean_server_root_for_vm(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    let store = platform_mbean_server_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&server) = store.get(&vm_identity) {
        out.push(server);
    }
    drop(store);
    // gen r5w3/obs7: and THIS VM's platform MemoryMXBean.
    let beans = platform_memory_mxbean_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&bean) = beans.get(&vm_identity) {
        out.push(bean);
    }
}

/// Remap THIS VM's platform MBeanServer with this VM's pointer map
/// (gc-common w8-f). Another VM's map describes another heap.
pub fn gc_update_platform_mbean_server_ref_for_vm(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    let mut store = platform_mbean_server_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(server) = store.get_mut(&vm_identity) {
        remap_platform_mbean_server(server, pointer_map);
    }
    drop(store);
    // gen r5w3/obs7: and THIS VM's platform MemoryMXBean.
    let mut beans = platform_memory_mxbean_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(bean) = beans.get_mut(&vm_identity) {
        remap_platform_mbean_server(bean, pointer_map);
    }
}

/// Drop VM `vm_identity`'s platform MBeanServer at VM teardown
/// (`release_vm_native_state`, through `handoff-w8f-jmx-root-row-is-vm-scoped`):
/// a raw heap `ObjectRef` must not outlive its heap, nor be handed to a later
/// VM that reuses the identity. Idempotent.
pub fn forget_vm_platform_mbean_server(vm_identity: usize) {
    platform_mbean_server_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm_identity);
    // gen r5w3/obs7: the VM's platform MemoryMXBean with it.
    platform_memory_mxbean_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&vm_identity);
    // gc-common w16-f: the VM's JMM switches go at the same teardown point
    // (this is the one per-VM teardown hook this file already has).
    JMM_SWITCHES.forget(vm_identity);
    // gc-common w17-e: the VM's start origin and its `-verbose:gc` switch.
    VM_CLOCKS.forget(vm_identity);
    cratonvm_native_builtins::phases_late::management::forget_vm_management_state(vm_identity);
}

/// The process-wide fallback origin (see [`PROCESS_START`]).
pub(in crate::jmx) fn process_start() -> &'static Instant {
    PROCESS_START.get_or_init(Instant::now)
}

pub(in crate::jmx) fn wall_clock_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(in crate::jmx) fn process_start_epoch_ms() -> u64 {
    *PROCESS_START_EPOCH_MS.get_or_init(|| {
        // Ensure PROCESS_START is initialised too
        let _ = process_start();
        wall_clock_now_ms()
    })
}

/// VM `vm`'s clock for start instant `start`: the recorded one, or a new one
/// whose wall-clock millis are derived now (`now - start.elapsed()`) and kept.
/// A row whose instant differs (an identity reused after a teardown that
/// skipped the hook) is replaced, never answered.
pub(in crate::jmx) fn clock_for_vm(vm: usize, start: Instant) -> VmClock {
    VM_CLOCKS.with(vm, |row| {
        if let Some(clock) = *row {
            if clock.start == start {
                return clock;
            }
        }
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let clock = VmClock {
            start,
            start_epoch_ms: wall_clock_now_ms().saturating_sub(elapsed_ms),
        };
        *row = Some(clock);
        clock
    })
}

/// The calling VM's start origin (gc-common w17-e): its own when the context
/// reports one, else the process-wide fallback.
pub(in crate::jmx) fn vm_clock(ctx: &dyn NativeContext) -> VmClock {
    match ctx.vm_start_instant() {
        Some(start) => clock_for_vm(ctx.vm_identity(), start),
        None => VmClock {
            start: *process_start(),
            start_epoch_ms: process_start_epoch_ms(),
        },
    }
}

/// `RuntimeMXBean.getStartTime()` / `VMManagementImpl.getStartupTime()`:
/// the calling VM's start, epoch millis.
pub(in crate::jmx) fn vm_start_epoch_ms(ctx: &dyn NativeContext) -> u64 {
    vm_clock(ctx).start_epoch_ms
}

/// `RuntimeMXBean.getUptime()`: millis since the calling VM's start.
pub(in crate::jmx) fn uptime_ms(ctx: &dyn NativeContext) -> u64 {
    vm_clock(ctx).start.elapsed().as_millis() as u64
}

// ---------------------------------------------------------------------------
// Verbose flags (JVM-wide in HotSpot, so per VM here)
//
// `-verbose:gc` / `-verbose:class` are JVM-wide switches in HotSpot, not
// per-bean state, and the JMM contract is that `setVerbose(b)` is observable
// through `isVerbose()`. Both round-trips used to be broken: the setters
// (`MemoryImpl.setVerboseGC`, `ClassLoadingImpl.setVerboseClass`) discarded
// their argument and the getters (`MemoryImpl.isVerbose`,
// `VMManagementImpl.getVerboseGC`/`getVerboseClass`) answered a hard-coded
// `false`, so `setVerbose(!isVerbose())` left the bean unchanged.
//
// `-verbose:class` is state-only: nothing here switches class-load tracing.
// That is the same trade-off `ClassLoadingMXBean.isVerbose` already documents
// further down this file, and a smaller lie than dropping the caller's write
// entirely. `-verbose:gc` is not (gc-common w18-g): it starts at the VM's
// launch switch and a write also reaches the VM's GC logging -- see
// `phases_late::management::verbose_gc_of` / `set_verbose_gc_of`.
// ---------------------------------------------------------------------------

/// The JMM switches a management client can flip, ONE SET PER VM.
///
/// gc-common w16-f: these were four process-wide `AtomicBool`s, so in a
/// process with several VMs (an embedder, the `vm` test binary) one VM's
/// `setThreadCpuTimeEnabled(false)` made every other VM's CPU-time getters
/// answer `-1`, and one VM's `setThreadContentionMonitoringEnabled(true)`
/// was read by the others as already enabled -- so THEIR enable transition
/// never reset their contention counters. HotSpot's switches are JVM-wide,
/// which in this process means VM-wide. Dropped with the VM's platform
/// MBeanServer at teardown ([`forget_vm_platform_mbean_server`]).
///
/// `-verbose:gc` is NOT here: it lives in `phases_late::management`
/// (`verbose_gc_of` / `set_verbose_gc_of`), shared with the synthetic
/// `MemoryMXBean` surface, which is compiled without the `management` feature
/// this module needs. Per VM too since gc-common w17-e.
pub(in crate::jmx) struct JmmSwitches {
    /// `ThreadMXBean.setThreadCpuTimeEnabled` state.
    ///
    /// Defaults to `true`, matching HotSpot, where thread CPU time is enabled
    /// out of the box on every platform that supports it. The OS scheduler's
    /// accounting genuinely cannot be switched off — but the JMM API can, and a
    /// caller that disables measurement must then see
    /// `isThreadCpuTimeEnabled()` go false and the getters return the `-1`
    /// sentinel. Treating "the clock always runs" as "the setter is a no-op"
    /// conflates the platform with the contract.
    pub(in crate::jmx) thread_cpu_time: bool,
    /// `com.sun.management.ThreadMXBean.setThreadAllocatedMemoryEnabled`
    /// state. Defaults to `true` for the same reason and with the same caveat
    /// as `thread_cpu_time`: HotSpot boots with thread allocated-memory
    /// measurement on, the underlying accounting (a TLAB cursor) genuinely
    /// cannot be switched off, but the API's `setEnabled(false)` must still be
    /// visible through `isEnabled()`.
    pub(in crate::jmx) thread_allocated_memory: bool,
    /// JMM contention monitoring is opt-in, like HotSpot. VM-side counters are
    /// reset at each enable transition so pre-enable lock activity is never
    /// leaked into the observable ThreadInfo values.
    pub(in crate::jmx) thread_contention_monitoring: bool,
    /// `-verbose:class`. Written by `ClassLoadingImpl.setVerboseClass`, read
    /// back by `VMManagementImpl.getVerboseClass` (which is what the real
    /// `ClassLoadingImpl.isVerbose()` bytecode calls through `this.jvm`).
    pub(in crate::jmx) verbose_class: bool,
}

impl Default for JmmSwitches {
    fn default() -> Self {
        Self {
            thread_cpu_time: true,
            thread_allocated_memory: true,
            thread_contention_monitoring: false,
            verbose_class: false,
        }
    }
}

pub(in crate::jmx) static JMM_SWITCHES: cratonvm_native_api::vm_scoped::VmScoped<JmmSwitches> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Read one of the calling VM's JMM switches without creating its row.
pub(in crate::jmx) fn jmm_switch(ctx: &dyn NativeContext, read: impl Fn(&JmmSwitches) -> bool) -> bool {
    JMM_SWITCHES
        .peek(ctx.vm_identity(), &read)
        .unwrap_or_else(|| read(&JmmSwitches::default()))
}

/// Write the calling VM's JMM switches; answers what `write` returns.
pub(in crate::jmx) fn jmm_switch_set<R>(ctx: &dyn NativeContext, write: impl FnOnce(&mut JmmSwitches) -> R) -> R {
    JMM_SWITCHES.with(ctx.vm_identity(), write)
}

/// The calling VM's `-verbose:gc`. Deliberately the SAME per-VM cell the
/// synthetic-mode `MemoryMXBean.setVerbose`/`isVerbose` pair in
/// `phases_late::management` uses, so a write through either surface is
/// visible from the other -- and, since gc-common w17-e, from no other VM.
pub(in crate::jmx) fn verbose_gc_get(ctx: &dyn NativeContext) -> bool {
    cratonvm_native_builtins::phases_late::management::verbose_gc_of(ctx)
}

pub(in crate::jmx) fn verbose_gc_set(ctx: &dyn NativeContext, on: bool) {
    cratonvm_native_builtins::phases_late::management::set_verbose_gc_of(ctx, on);
}

/// Read the `boolean` argument of a `(Z)V` setter.
///
/// Deliberately position-independent. `MemoryImpl.setVerboseGC` and
/// `ClassLoadingImpl.setVerboseClass` are declared STATIC in the JDK (the
/// instance `setVerbose` wrappers are what call them), so the flag is arg 0 —
/// but a CratonVM synthetic receiver can reach the same native with `this`
/// prepended, putting it at arg 1. The first `Value::Int` is the flag under
/// either convention, because a receiver is always a `Value::Object`. Reading
/// a fixed index would silently store `false` under the other one.
pub(in crate::jmx) fn bool_flag_arg(args: &[Value]) -> bool {
    args.iter()
        .find_map(|v| match v {
            Value::Int(v) => Some(*v != 0),
            _ => None,
        })
        .unwrap_or(false)
}

/// The 1-minute system load average, or the JMM's `-1.0` "not available"
/// sentinel on platforms that do not publish one.
///
/// `OperatingSystemMXBean.getSystemLoadAverage()` returned a flat `-1.0`
/// everywhere even though Linux exposes the real number in `/proc/loadavg`
/// (the sibling `phases_late::management` registration already read it).
pub(in crate::jmx) fn system_load_average() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
            if let Some(first) = loadavg.split_whitespace().next() {
                if let Ok(avg) = first.parse::<f64>() {
                    return avg;
                }
            }
        }
    }
    -1.0
}

/// The `()J` OS metrics under their **JDK 8 / legacy** `*0` native names.
///
/// Returned as a table rather than registered inline so the JDK 9+
/// `com.sun.management.internal.OperatingSystemImpl` copy and the public
/// interface methods can be built from the same list — a metric added here
/// cannot be forgotten on one of the three surfaces.
pub(in crate::jmx) type OsMetricNative = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

pub(in crate::jmx) fn os_metric_long_natives() -> [(&'static str, OsMetricNative); 8] {
    [
        ("getCommittedVirtualMemorySize0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::committed_virtual_memory())))
        }),
        ("getTotalSwapSpaceSize0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::total_swap())))
        }),
        ("getFreeSwapSpaceSize0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::free_swap())))
        }),
        ("getProcessCpuTime0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::process_cpu_time_ns())))
        }),
        ("getFreePhysicalMemorySize0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::free_physical_memory())))
        }),
        ("getTotalPhysicalMemorySize0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::total_physical_memory())))
        }),
        ("getOpenFileDescriptorCount0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::open_fd_count())))
        }),
        ("getMaxFileDescriptorCount0", |_ctx, _args| {
            Ok(Some(Value::Long(os_metrics::max_fd_count())))
        }),
    ]
}

/// The same eight metrics under their **JDK 9+** `*0` native names. JDK 25
/// renamed three of them (`getFreePhysicalMemorySize0` -> `getFreeMemorySize0`,
/// `getTotalPhysicalMemorySize0` -> `getTotalMemorySize0`, and on the load side
/// `getSystemCpuLoad0` -> `getCpuLoad0`); the rest are unchanged.
pub(in crate::jmx) fn os_metric_long_natives_jdk9() -> [(&'static str, OsMetricNative); 8] {
    let mut table = os_metric_long_natives();
    for entry in &mut table {
        entry.0 = match entry.0 {
            "getFreePhysicalMemorySize0" => "getFreeMemorySize0",
            "getTotalPhysicalMemorySize0" => "getTotalMemorySize0",
            other => other,
        };
    }
    table
}

/// The VM's boot class path, as published in the `sun.boot.class.path` system
/// property, or `None` when the VM never established one.
///
/// This is the single source both `RuntimeMXBean.getBootClassPath()` and
/// `isBootClassPathSupported()` read, which is what keeps them from
/// contradicting each other. Note that "present but EMPTY" is a real answer,
/// not an absence: on a modular JDK with nothing appended via
/// `-Xbootclasspath/a` HotSpot reports exactly that. Absence means the property
/// was never populated at all, and only then must `isBootClassPathSupported()`
/// report false — otherwise a caller would take the empty string for a
/// measurement.
pub(in crate::jmx) fn boot_class_path(ctx: &dyn NativeContext) -> Option<String> {
    ctx.get_system_property("sun.boot.class.path")
}

/// `jmm.h`'s `JMM_VMGLOBAL_ORIGIN_ENVIRON_VAR` — the flag value came from an
/// environment variable, which is precisely how every CratonVM flag is set.
/// `Flag.getVMOption()` maps this to `VMOption.Origin.ENVIRON_VAR`, and its
/// `switch` has a `default` arm, so an unrecognised code degrades to
/// `Origin.OTHER` rather than throwing.
pub(in crate::jmx) const JMM_VMGLOBAL_ORIGIN_ENVIRON_VAR: i32 = 4;

/// The real JDK constructor `getFlags` builds its results with:
/// `Flag(String name, Object value, boolean writeable, boolean external, int origin)`.
/// Every use of it below is guarded by a `method_exists` check, so if a future
/// JDK reshapes it the flag surface degrades to the previous "no flags"
/// answer instead of misreporting one.
pub(in crate::jmx) const FLAG_INIT_DESC: &str = "(Ljava/lang/String;Ljava/lang/Object;ZZI)V";

/// `com.sun.management.internal.Flag`'s real JDK class name.
pub(in crate::jmx) const FLAG_CLASS: &str = "com/sun/management/internal/Flag";

/// Every CratonVM VM flag that is actually SET in this process, as
/// `(name, raw value)` pairs sorted by name.
///
/// The name set is the declared flag inventory in `cratonvm_types::flag_groups`
/// — the ten grouped variables, the scalars, and every legacy per-knob key a
/// group token expands to — i.e. the same set `flags::declared_flag_names()`
/// gates on. Values come through `flags::runtime_var`, which reads the one
/// latched `VmFlags` snapshot rather than live `environ`, so this reports what
/// the VM is actually running with, including keys set indirectly by a grouped
/// expression such as `CRATONVM_JIT=no-bce`.
///
/// Flags left at their built-in default are deliberately ABSENT rather than
/// listed with a fabricated default: an inventory row records a knob's name and
/// group but not the predicate its owner parses it with (`truthy_word`,
/// `present`, `on_unless_zero`, …), so "the default value of this flag" is not
/// derivable here without guessing one. Reporting only the flags carrying an
/// explicit value is what `-XX:+PrintCommandLineFlags` shows, and every entry
/// in it is exact.
pub(in crate::jmx) fn craton_vm_flag_settings() -> Vec<(String, String)> {
    use cratonvm_types::flag_groups;
    let mut names: Vec<&'static str> = Vec::new();
    names.extend(
        flag_groups::Group::ALL
            .iter()
            .copied()
            .map(flag_groups::Group::var),
    );
    names.extend(flag_groups::SCALARS.iter().copied());
    for entry in flag_groups::INVENTORY {
        names.extend(entry.on_key);
        names.extend(entry.off_key);
    }
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            cratonvm_types::flags::runtime_var(name)
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

/// zt t6/tc: HotSpot's manageable `SoftMaxHeapSize` (a `size_t` flag, so its
/// JMX value is a `java.lang.Long`).
pub(in crate::jmx) const SOFT_MAX_FLAG: &str = "SoftMaxHeapSize";

/// zt t6/tc: the `SoftMaxHeapSize` row's state on a ZGC heap: the soft
/// maximum in bytes, `-Xmx` (the constraint's bound) and the origin kind the
/// heap recorded (0 ergonomic default, 1 command line, 2 management).
#[derive(Clone, Copy)]
pub(in crate::jmx) struct SoftMaxRow {
    pub(in crate::jmx) value: u64,
    pub(in crate::jmx) max: u64,
    pub(in crate::jmx) origin: u8,
}

impl SoftMaxRow {
    /// `jmm.h`'s `jmmVMGlobalOrigin` for the row: `ERGONOMIC` (6) for the
    /// default (ZGC sets the soft maximum to `MaxHeapSize` ergonomically),
    /// `COMMAND_LINE` (2) for `-XX:SoftMaxHeapSize`, `MANAGEMENT` (3) after
    /// a `setVMOption`. The default's spelling is decided by the probe row
    /// `Zt6TcSoftMaxManageable` against HotSpot (`getOrigin()`).
    pub(in crate::jmx) fn jmm_origin(self) -> i32 {
        match self.origin {
            1 => 2,
            2 => 3,
            _ => 6,
        }
    }

    /// zt t7/tc2: the same origin as the `com.sun.management.VMOption.Origin`
    /// constant JDK 9+'s `Flag` constructor takes (HotSpot's `Flag.c` maps
    /// `COMMAND_LINE` to `VM_CREATION`). HotSpot 25.0.3 reports `ERGONOMIC`
    /// for ZGC's default and `MANAGEMENT` after a `setVMOption` (row SM1).
    pub(in crate::jmx) fn origin_name(self) -> &'static str {
        match self.origin {
            1 => "VM_CREATION",
            2 => "MANAGEMENT",
            _ => "ERGONOMIC",
        }
    }
}

/// zt t7/tc2: the constructor JDK 9+ (25 included) actually declares,
/// `Flag(String name, Object value, boolean writeable, boolean external,
/// VMOption.Origin origin)` (`javap -p --module jdk.management
/// com.sun.management.internal.Flag` on 25.0.3). [`FLAG_INIT_DESC`]'s `int`
/// origin shape does not exist there, so `native_flag_get_flags` returned 0
/// for every name and `getVMOption("SoftMaxHeapSize")` answered "does not
/// exist" in both modes (SM1 on the t6 build).
pub(in crate::jmx) const FLAG_INIT_DESC_ORIGIN: &str =
    "(Ljava/lang/String;Ljava/lang/Object;ZZLcom/sun/management/VMOption$Origin;)V";

/// `com.sun.management.VMOption.Origin`.
pub(in crate::jmx) const VM_OPTION_ORIGIN_CLASS: &str = "com/sun/management/VMOption$Origin";

/// The `VMOption.Origin` enum constant named `name` (initialising the enum
/// if needed), or `None` when the class or constant cannot be reached.
pub(in crate::jmx) fn vm_option_origin(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let class_id = ctx.ensure_class_initialized(VM_OPTION_ORIGIN_CLASS).ok()?;
    let idx = ctx.static_field_index_by_name(class_id, name)?;
    match ctx.get_static_field(class_id, idx) {
        Value::Object(obj) => obj,
        _ => None,
    }
}

/// zt t6/tc: `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE` (default ON since the zt t7
/// flip, `=0` kill; row SM1 SAME as HotSpot 25.0.3 in both modes): on a ZGC heap the
/// `com.sun.management.internal.Flag` surface lists `SoftMaxHeapSize` as a
/// writeable `Long` and `setLongValue` applies it to the heap, as HotSpot's
/// `HotSpotDiagnosticMXBean.setVMOption("SoftMaxHeapSize", ..)` does (the
/// director re-reads it; here `ZgcRealHeap::set_soft_max_heap_bytes` is two
/// relaxed stores read by every trigger poll). `None` off ZGC, with the switch
/// off, or from a context with no VM heap.
pub(in crate::jmx) fn soft_max_flag_row(ctx: &dyn NativeContext) -> Option<SoftMaxRow> {
    let (value, max, origin) = ctx.zgc_soft_max_heap_flag()?;
    Some(SoftMaxRow { value, max, origin })
}

/// [`craton_vm_flag_settings`] plus the `SoftMaxHeapSize` row when there is
/// one, still sorted by name, so `getInternalFlagCount`, `getAllFlagNames`
/// and `getFlags(null, ..)` agree.
pub(in crate::jmx) fn flag_rows_with_soft_max(soft_max: Option<SoftMaxRow>) -> Vec<(String, String)> {
    let mut rows = craton_vm_flag_settings();
    if let Some(row) = soft_max {
        rows.push((SOFT_MAX_FLAG.to_string(), row.value.to_string()));
        rows.sort_by(|a, b| a.0.cmp(&b.0));
    }
    rows
}

/// The same resolved HotSpot rows used by PrintFlagsFinal and VM.flags,
/// merged with the VM's own explicit environment settings and live soft max.
pub(in crate::jmx) fn resolved_vm_flag_settings(ctx: &dyn NativeContext) -> Vec<(String, String)> {
    let mut rows = flag_rows_with_soft_max(soft_max_flag_row(ctx));
    for flag in ctx.hotspot_vm_flags() {
        if !rows.iter().any(|(name, _)| name == flag.name) {
            rows.push((flag.name.to_string(), flag.value));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// zt t24/x20: the rows `getFlags(null, ..)` actually lists -- the ones
/// [`native_flag_get_flags`] constructs a `Flag` for. On a JDK whose `Flag`
/// has no `int`-origin constructor (JDK 9+, 25 included) a CratonVM-only row is
/// skipped there, so `getInternalFlagCount` and `getAllFlagNames` counted and
/// named rows `getVMOption` then answered "does not exist" for (the merge of
/// dev cabdb9f4ae widened the count to every resolved row).
pub(in crate::jmx) fn listed_vm_flag_settings(ctx: &dyn NativeContext) -> Vec<(String, String)> {
    let rows = resolved_vm_flag_settings(ctx);
    if ctx.method_exists(FLAG_CLASS, "<init>", FLAG_INIT_DESC) {
        return rows;
    }
    let hotspot = ctx.hotspot_vm_flags();
    rows.into_iter()
        .filter(|(name, _)| {
            name == SOFT_MAX_FLAG || hotspot.iter().any(|row| row.name == name.as_str())
        })
        .collect()
}

/// zt t24/x20: `CRATONVM_JMX_MANAGEABLE_HEAP_FREE_RATIOS` (default ON, `=0`
/// kill; every collector, both modes). A modelled HotSpot row whose kind is
/// `{manageable}` (`MinHeapFreeRatio`, `MaxHeapFreeRatio`) is listed
/// `writeable` and `external`, as jmm's `GetVMGlobals` lists a manageable flag,
/// and `setVMOption` writes it (`NativeSystemAccess::set_manageable_hotspot_flag`);
/// ZGC's `SoftMaxHeapSize` row is `external` too. So
/// `HotSpotDiagnosticMXBean.getDiagnosticOptions()` (writeable AND external)
/// lists them, as on HotSpot. `=0`: every modelled row read-only and
/// non-external, `SoftMaxHeapSize` writeable but not external, as before.
pub(in crate::jmx) fn jmx_manageable_heap_free_ratios() -> bool {
    true
}

/// `Flag.setLongValue` -- zt t6/tc: applies `SoftMaxHeapSize` on a ZGC heap
/// unless `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE=0` (see [`soft_max_flag_row`]);
/// every other name is [`native_flag_set_value_unsupported`]. A value above
/// `-Xmx` is refused with an `IllegalArgumentException` (HotSpot's
/// `SoftMaxHeapSize` constraint, with HotSpot 25.0.3's message since zt
/// t10/tg3).
pub(in crate::jmx) fn native_flag_set_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut name: Option<String> = None;
    for arg in args {
        if let Value::Object(Some(obj)) = arg {
            name = ctx.read_string(*obj);
            break;
        }
    }
    let value = args.iter().find_map(|v| match v {
        Value::Long(n) => Some(*n),
        _ => None,
    });
    match (name.as_deref(), value, soft_max_flag_row(&*ctx)) {
        (Some(SOFT_MAX_FLAG), Some(bytes), Some(row)) => {
            if bytes < 0 || bytes as u64 > row.max {
                // zt t10/tg3: HotSpot 25.0.3's text (local `-Xmx64m`:
                // `setVMOption("SoftMaxHeapSize", "134217728")` and
                // `"67108865"` both throw `SoftMaxHeapSize error: value
                // violates its flag's constraint.`; a negative long is a huge
                // `size_t` there, the same constraint). It read
                // `VM Option "SoftMaxHeapSize": <n> is outside 0..=<max>`.
                return Err(RuntimeError::IllegalArgumentException {
                    message: "SoftMaxHeapSize error: value violates its flag's constraint."
                        .to_string(),
                }
                .into());
            }
            // Cast: `bytes >= 0` was checked above.
            ctx.zgc_set_soft_max_heap_flag(bytes as u64);
            Ok(None)
        }
        // zt t24/x20 (`CRATONVM_JMX_MANAGEABLE_HEAP_FREE_RATIOS`): a modelled
        // manageable row. The `long` is HotSpot's unsigned flag value (a
        // negative one is a huge `uintx` there, outside the range).
        (Some(flag), Some(raw), _) if jmx_manageable_heap_free_ratios() => {
            // Cast: two's-complement reinterpretation, as jmm's `uintx` store.
            match ctx.set_manageable_hotspot_flag(flag, raw as u64) {
                Some(Ok(())) => Ok(None),
                Some(Err(message)) => {
                    Err(RuntimeError::IllegalArgumentException { message }.into())
                }
                None => native_flag_set_value_unsupported(ctx, args),
            }
        }
        _ => native_flag_set_value_unsupported(ctx, args),
    }
}

/// CPU and user time consumed by the CALLING thread, in nanoseconds.
///
/// `(total_cpu_ns, user_ns)`, or `None` on a platform whose per-thread clock we
/// do not read (the JMM's "measurement unavailable" case, which the callers map
/// to `-1`). The numbers come from the OS scheduler's own accounting for the
/// current OS thread — no VM-side bookkeeping is involved, which is exactly why
/// this needs no VM-side bookkeeping at all. ARBITRARY-thread CPU time now
/// works too — see `management::os_thread_cpu_time_ns`, reached from a Java
/// `tid` via `NativeContext::thread_os_tid` — so the two are no longer
/// asymmetric; this one is kept separate only because the caller's own clock is
/// readable without an `OpenThread` / `/proc` round trip.
pub(in crate::jmx) fn current_thread_cpu_time_ns() -> Option<(i64, i64)> {
    #[cfg(target_os = "windows")]
    {
        // Delegates so the `GetThreadTimes` FFI is declared exactly once in the
        // crate — the arbitrary-thread read next door needs the same signature
        // plus `OpenThread`/`CloseHandle`, and two `extern` blocks for one
        // symbol is how those drift apart.
        cratonvm_native_builtins::phases_late::management::windows_current_thread_cpu_time_ns()
    }
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            // RUSAGE_THREAD accounts the CALLING thread only, which is the JMM's
            // per-thread contract; RUSAGE_SELF would sum the whole process.
            if libc::getrusage(libc::RUSAGE_THREAD, &mut usage) == 0 {
                let to_ns = |t: libc::timeval| {
                    (t.tv_sec as i64) * 1_000_000_000 + (t.tv_usec as i64) * 1_000
                };
                let user_ns = to_ns(usage.ru_utime);
                return Some((user_ns + to_ns(usage.ru_stime), user_ns));
            }
        }
        None
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

/// Whether `requested` names the CALLING thread — i.e. whether the OS
/// per-thread clock [`current_thread_cpu_time_ns`] reads is the right one to
/// answer a `getThreadCpuTime(long)` / `getThreadUserTime(long)` call with.
///
/// `None` (no `long` in the argument list) is treated as "the current thread",
/// the same fallback the `getThreadInfo(J)` native below takes. The comparison
/// is against the live `java.lang.Thread.tid`, which is the identity
/// `getAllThreadIds()` hands out and `getThreadInfo(long)` resolves, so an id
/// obtained from either round-trips here.
pub(in crate::jmx) fn current_thread_tid_matches(ctx: &mut dyn NativeContext, requested: Option<i64>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    let current = ctx.current_thread_object();
    // Round 15 wave 4 (lane threadvm): `Thread`'s own `tid` here and in every
    // id match below (`lang_misc::thread_field`), not a subclass's `long tid`.
    match cratonvm_native_builtins::lang_misc::thread_field(ctx, current, "tid") {
        Value::Long(id) => id == requested,
        Value::Int(id) => i64::from(id) == requested,
        _ => false,
    }
}

/// `(cpu_ns, user_ns)` for the thread whose Java `tid` is `requested`, for the
/// `getThreadCpuTime(long)` / `getThreadUserTime(long)` family.
///
/// The caller's own id (and `None`, i.e. "no id supplied") still goes through
/// [`current_thread_cpu_time_ns`]: that is the cheaper and more precise clock,
/// and routing it here is what keeps the per-id getters agreeing with
/// `getCurrentThreadCpuTime()`. Any other id resolves the Java `Thread` mirror,
/// asks the VM for its OS tid, and reads that tid's platform clock. `None`
/// anywhere along the way is the JMM's "not available", which callers map to
/// `-1` — a thread that has already exited is exactly that case.
pub(in crate::jmx) fn cpu_time_for_requested_tid(
    ctx: &mut dyn NativeContext,
    requested: Option<i64>,
) -> Option<(i64, i64)> {
    // `setThreadCpuTimeEnabled(false)` must actually stop the measurement, not
    // merely be recorded. Gated here rather than at each getter because this is
    // the single choke point every one of them funnels through — gating them
    // individually is how one gets missed.
    if !jmm_switch(ctx, |s| s.thread_cpu_time) {
        return None;
    }
    if current_thread_tid_matches(ctx, requested) {
        return current_thread_cpu_time_ns();
    }
    cpu_time_of_java_tid(ctx, requested?)
}

/// Pull the `long` thread id out of a native argument list.
///
/// A caller reaching a native through a JIT'd or reflective path can present a
/// `long` as `Value::Int`, so both are accepted; `None` means no id was passed
/// and the caller is asking about itself.
pub(in crate::jmx) fn requested_thread_id(args: &[Value]) -> Option<i64> {
    args.iter().find_map(|arg| match arg {
        Value::Long(id) => Some(*id),
        _ => None,
    })
}

pub(in crate::jmx) fn native_set_thread_contention_monitoring_enabled(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let enabled = args.iter().any(|value| matches!(value, Value::Int(1)));
    let was_enabled = jmm_switch_set(ctx, |s| {
        std::mem::replace(&mut s.thread_contention_monitoring, enabled)
    });
    if enabled && !was_enabled {
        ctx.reset_thread_jmx_contention_stats();
    }
    Ok(None)
}

/// JVMTI/JMM thread-status bit for "blocked entering a monitor", the value
/// `vm_exec::thread_jmx_snapshot` writes into `ThreadJmxSnapshot::thread_status`
/// for a thread contending for a monitor (its `WAITING` sibling is `0x0010`).
pub(in crate::jmx) const JMM_THREAD_STATUS_BLOCKED_ON_MONITOR_ENTER: i32 = 0x0400;

/// JMM's `JMM_THREAD_STATE_FLAG_NATIVE` in `ThreadJmxSnapshot::thread_status`:
/// the thread runs native code (`ThreadInfo.isInNative()`).
pub(in crate::jmx) const JMM_THREAD_STATE_FLAG_NATIVE: i32 = 0x0040_0000;

/// Wait-for-graph deadlock detection, shared by the four `find*DeadlockedThreads`
/// registrations (`ThreadMXBean` and `sun.management.ThreadImpl`).
///
/// The edge set comes from the VM's own per-thread JMX snapshot:
/// `ThreadJmxSnapshot::lock_owner_id` is the id of the thread that owns the lock
/// the snapshotted thread is blocked on — i.e. exactly one wait-for edge. Every
/// thread lying on a cycle is deadlocked, which is the JMM's own definition, so
/// this is a real detector rather than the previous unconditional "we found
/// none". `None` (the spec's `null`, not an empty array) when no cycle exists.
///
/// Only a thread BLOCKED ENTERING a monitor contributes an edge — see
/// [`JMM_THREAD_STATUS_BLOCKED_ON_MONITOR_ENTER`]. `vm_exec::thread_jmx_snapshot`
/// reports `lock` as `contended.or(waiting)`, so without that filter a thread
/// sitting in `Object.wait()` would contribute an edge to whichever thread
/// re-acquired the monitor it released — and two threads in `wait()` on each
/// other's monitors would be reported as a deadlock even though a `notify()`
/// frees them. The JMM defines `findMonitorDeadlockedThreads()` over threads
/// "blocked waiting to ENTER" a monitor, which is exactly this filter.
///
/// LIVE as of 2026-07-28 — the caveat that used to sit here ("the edge set is
/// always EMPTY") is obsolete. `vm_exec::monitor_enter_blocking` (vm_exec.rs
/// 1470/1538) and `monitor_enter_synchronized_method` (1571/1628) now call
/// `set_jmx_contended_monitor` / `complete_jmx_monitor_enter`, and
/// `thread_jmx_snapshot` resolves `lock_owner_id` from the monitor's current
/// owner, so a genuine wait-for edge is observable.
///
/// Ownership is now published on the UNCONTENDED path as well (the `None` arm
/// of both functions, and `invoke_fast`'s door), so `getLockedMonitors()` sees
/// thin locks too; the residual sites that acquire without publishing are
/// listed in `r11w4-sync-jmx-object-monitor-usage-surfaces-disagree-FIXED-20260924`.
/// That is harmless for THIS detector either way — a deadlocked thread is by
/// definition contending, and a contended acquisition always publishes.
/// `Object.wait()` publishes a waited-on monitor (`set_jmx_waiting_monitor`),
/// and the BLOCKED_ON_MONITOR_ENTER filter above keeps it out of the edge set,
/// which is correct: the JMM defines this over threads blocked ENTERING a
/// monitor. A contender that is still spinning before it parks
/// (`MonitorTable::enter_or_contend`) is RUNNABLE and contributes no edge
/// until the bounded spin gives up and it blocks.
///
/// Interpreter round i4 wave 84, lane L6: `with_synchronizers` (the
/// `findDeadlockedThreads` doors) adds HotSpot's second edge kind
/// (`ThreadService::find_deadlocks_at_safepoint(concurrent_locks = true)`): a
/// thread not blocked on a monitor whose `Thread.parkBlocker` is an
/// `AbstractOwnableSynchronizer` waits for that synchronizer's
/// `exclusiveOwnerThread`. Without it a `ReentrantLock` deadlock -- or a mixed
/// cycle through one -- was never found (`L6W84DeadlockDetection`).
/// `findMonitorDeadlockedThreads` keeps the monitor edges only, as HotSpot's.
pub(in crate::jmx) fn deadlocked_thread_ids(ctx: &mut dyn NativeContext, with_synchronizers: bool) -> Option<Vec<i64>> {
    // Every thread in enumeration order, with the one thread it waits for.
    let mut nodes: Vec<(i64, Option<i64>)> = Vec::new();
    for thread in ctx.enumerate_threads(usize::MAX) {
        // Interpreter round i2 wave 61, lane L6 (`CRATONVM_THREAD_INFO_FRAMELESS`):
        // the wait-for graph needs no stack, so none is captured.
        if let Some(snapshot) = ctx.thread_jmx_snapshot_frameless(thread) {
            if snapshot.thread_id <= 0 {
                continue;
            }
            let blocked_on_enter =
                (snapshot.thread_status & JMM_THREAD_STATUS_BLOCKED_ON_MONITOR_ENTER) != 0;
            let edge = if blocked_on_enter && snapshot.lock_owner_id > 0 {
                Some(snapshot.lock_owner_id)
            } else if with_synchronizers {
                parked_synchronizer_owner_tid(ctx, thread)
            } else {
                None
            };
            nodes.push((snapshot.thread_id, edge));
        }
    }
    // HotSpot walks its threads list, in start order; the registry enumerates
    // a hash map. `Thread.tid` order (creation) is the deterministic stand-in:
    // the same order for every thread started in the order it was created.
    nodes.sort_by_key(|&(tid, _)| tid);
    let deadlocked = hotspot_deadlock_walk(&nodes);
    if deadlocked.is_empty() {
        None
    } else {
        Some(deadlocked)
    }
}

/// HotSpot's `ThreadService::find_deadlocks_at_safepoint` over `nodes` (each
/// thread in list order with the thread it waits for), answering the threads
/// of every cycle found, in discovery order (interpreter round i5 wave 101,
/// lane L6; `L6W101DeadlockWalkShape`). Two properties of HotSpot's walk the
/// earlier "is `start` on a cycle" test did not have:
///
/// * a thread that waits for ITSELF (a non-reentrant `AbstractQueuedSynchronizer`
///   re-acquired by its owner) is a self-loop and is ignored, not reported;
/// * a cycle lists every thread of the walk that found it, so a thread earlier
///   in the list that waits for a deadlocked thread is reported with the cycle
///   (`cycle->add_thread` at each step of the walk from that thread).
pub(in crate::jmx) fn hotspot_deadlock_walk(nodes: &[(i64, Option<i64>)]) -> Vec<i64> {
    let index_of = |tid: i64| nodes.iter().position(|&(t, _)| t == tid);
    let mut dfn: Vec<i64> = vec![-1; nodes.len()];
    let mut global_dfn: i64 = 0;
    let mut deadlocked: Vec<i64> = Vec::new();
    for start in 0..nodes.len() {
        if dfn[start] >= 0 {
            continue;
        }
        let this_dfn = global_dfn;
        dfn[start] = global_dfn;
        global_dfn += 1;
        let mut cycle: Vec<i64> = Vec::new();
        let mut current = start;
        while let Some(owner) = nodes[current].1 {
            cycle.push(nodes[current].0);
            let Some(next) = index_of(owner) else {
                break;
            };
            if dfn[next] < 0 {
                dfn[next] = global_dfn;
                global_dfn += 1;
            } else if dfn[next] < this_dfn {
                // Visited by an earlier walk: not a new cycle.
                break;
            } else if next == current {
                // A self-loop: ignored, as HotSpot's walk does.
                break;
            } else {
                for tid in cycle.drain(..) {
                    if !deadlocked.contains(&tid) {
                        deadlocked.push(tid);
                    }
                }
                break;
            }
            current = next;
        }
    }
    deadlocked
}

/// The Java tid of the thread that owns the `AbstractOwnableSynchronizer`
/// `thread` is parked on (`Thread.parkBlocker`), as HotSpot's deadlock walk
/// reads `current_park_blocker()` and
/// `AbstractOwnableSynchronizer.exclusiveOwnerThread`. `None` for no blocker,
/// a blocker that is not an ownable synchronizer (a `ConditionObject`, a
/// `CountDownLatch`'s `Sync` with no owner, a `FutureTask`), or no owner.
/// Field reads only: allocates nothing.
pub(in crate::jmx) fn parked_synchronizer_owner_tid(ctx: &mut dyn NativeContext, thread: ObjectRef) -> Option<i64> {
    let Value::Object(Some(blocker)) = cratonvm_native_builtins::lang_misc::thread_field(&*ctx, thread, "parkBlocker")
    else {
        return None;
    };
    let is_aos = ctx
        .class_assignable_to_name(
            ctx.class_id_of_object(blocker),
            "java/util/concurrent/locks/AbstractOwnableSynchronizer",
        )
        .unwrap_or(false);
    if !is_aos {
        return None;
    }
    let Value::Object(Some(owner)) = ctx.get_field_by_name(blocker, "exclusiveOwnerThread") else {
        return None;
    };
    match cratonvm_native_builtins::lang_misc::thread_field(&*ctx, owner, "tid") {
        Value::Long(id) if id > 0 => Some(id),
        _ => None,
    }
}

/// `[J` form of [`deadlocked_thread_ids`] — the shared body of all four
/// `find*DeadlockedThreads` natives. `null` means "no deadlock", per the JMM.
pub(in crate::jmx) fn deadlocked_threads_result(
    ctx: &mut dyn NativeContext,
    with_synchronizers: bool,
) -> MethodCallResult {
    match deadlocked_thread_ids(ctx, with_synchronizers) {
        None => Ok(Some(Value::Object(None))),
        Some(ids) => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, ids.len());
            for (i, id) in ids.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Long(*id));
            }
            Ok(Some(Value::Object(Some(arr))))
        }
    }
}

/// `[Ljava/lang/Thread;` form of [`deadlocked_thread_ids`].
///
/// This is the shape the real `sun.management.ThreadImpl` natives declare —
/// verified against JDK 25 bytecode: `private static native Thread[]
/// findDeadlockedThreads0()`, whose result the Java side feeds to
/// `threadsToIds(Thread[])`. The `[J` form above is the `ThreadMXBean`
/// interface's return type and is NOT interchangeable with it. `null` still
/// means "no deadlock"; `threadsToIds` maps null straight through.
pub(in crate::jmx) fn deadlocked_threads_object_result(
    ctx: &mut dyn NativeContext,
    with_synchronizers: bool,
) -> MethodCallResult {
    let Some(ids) = deadlocked_thread_ids(ctx, with_synchronizers) else {
        return Ok(Some(Value::Object(None)));
    };
    let thread_cid = ctx
        .class_id_by_name("java/lang/Thread")
        .unwrap_or(ClassId::new(0));
    let arr = ctx.new_ref_array(thread_cid, ids.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, &id) in ids.iter().enumerate() {
        if let Some(thread) = thread_object_for_java_tid(ctx, id) {
            let arr_fresh = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr_fresh, i, Value::Object(Some(thread)));
        }
    }
    let arr_final = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(arr_final))))
}

// ---------------------------------------------------------------------------
// Public registration entry-point
// ---------------------------------------------------------------------------

pub fn register_jmx_natives(r: &mut NativeMethodRegistry) {
            register_object_name(r);
    register_object_instance(r);
    register_mbean_server(r);
    register_management_factory(r);
    register_runtime_mxbean(r);
    register_memory_mxbean(r);
    register_memory_usage(r);
    register_thread_mxbean(r);
    register_class_loading_mxbean(r);
    register_operating_system_mxbean(r);
    register_compilation_mxbean(r);
    register_virtual_thread_scheduler_mxbean(r);
    register_gc_mxbean(r);
    register_platform_logging_mxbean(r);
    // `PlatformManagedObject.getObjectName()` for every bean the calls above
    // stamp with an MXBean *interface*. Registered last so it cannot be
    // shadowed by an earlier registration of the same triple; none of the
    // functions above declare `getObjectName`, so nothing is overridden.
    register_platform_managed_object_names(r);
    // NOTE: `register_mbean_server` is called above as a `SyntheticStub`
    // fallback for runs where `ManagementFactory.getPlatformMBeanServer()`
    // returns the synthetic interface object. The real-JDK hazard is the
    // MBeanServerFactory synthetic override, not these fallback methods: when a
    // concrete `com.sun.jmx.mbeanserver.JmxMBeanServer` exists, its bytecode
    // should still win over SyntheticStub registrations.
    //
    // The original shadowing problem applies to
    // `register_mbean_server_factory_synthetic`
    // (the `MBeanServerFactory.createMBeanServer`/`newMBeanServer` overrides,
    // previously inlined into `register_management_factory` above and called
    // unconditionally by `register_jmx_natives` in BOTH real- and
    // synthetic-JDK registration branches). That was the actual live bug:
    // it shadowed `createMBeanServer` before real bytecode could construct
    // `JmxMBeanServer`, so `getPlatformMBeanServer()` handed out a
    // synthetic interface-typed server even in real-JDK mode. It is now a
    // standalone function that callers must invoke explicitly ONLY from the
    // synthetic-JDK registration path (see `vm_init.rs`) — this function
    // does NOT call it.
    register_vm_management_impl(r);
}

pub(in crate::jmx) fn register_object_name(r: &mut NativeMethodRegistry) {
    // JDK-ONLY-WAVE2: pinned. This registrar previously set no category of its
    // own and inherited `Bridge` from `register_jmx_natives`'s window -- the
    // last ambient-category dependency in the JMX surface, and the one the
    // `JMX real path` P0 row names as step 1. `javax/management/ObjectName` is
    // the very class the reverted retag NPE'd on, so the tag being *right by
    // inheritance* was a coincidence waiting to break the next time a caller
    // moved. Behaviour is unchanged: the only caller already opens `Bridge`.
            let cls = "javax/management/ObjectName";
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;)V",
        native_object_name_init_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        native_object_name_init_domain_key_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Ljava/util/Hashtable;)V",
        native_object_name_init_domain_table, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/management/ObjectName;",
        native_object_name_get_instance_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getInstance",
        "(Ljavax/management/ObjectName;)Ljavax/management/ObjectName;",
        native_object_name_get_instance_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "quote",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_object_name_quote, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getCanonicalName",
        "()Ljava/lang/String;",
        native_object_name_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "toString",
        "()Ljava/lang/String;",
        native_object_name_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getCanonicalName",
        "()Ljava/lang/String;",
        native_object_name_get_canonical_name, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getDomain",
        "()Ljava/lang/String;",
        native_object_name_domain, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getKeyProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_object_name_get_key_property, cratonvm_native_api::NativeKind::Bridge,
    );
    // RKC-ObjectName-02: this synthetic ObjectName deliberately stores only
    // its source text, so all three key-property accessors must stay native.
    // Their real JDK implementations read `_ca_array` / `_kp_array` (and the
    // private lazy `_propertyList`) which do not exist in the one-field model.
    // Keep the public defensive Hashtable, the private Map used by
    // getKeyProperty, and the source-order list-string accessor together: an
    // uncovered member otherwise falls through to real bytecode and NPEs.
    r.register(
        cls,
        "_getKeyPropertyList",
        "()Ljava/util/Map;",
        native_object_name_get_key_property_map, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getKeyPropertyList",
        "()Ljava/util/Hashtable;",
        native_object_name_get_key_property_list, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getKeyPropertyListString",
        "()Ljava/lang/String;",
        native_object_name_get_key_property_list_string, cratonvm_native_api::NativeKind::Bridge,
    );
    // RKC-ObjectName-01: `getCanonicalKeyPropertyListString` and the
    // `is*Pattern` family are real, un-intercepted-until-now `ObjectName`
    // methods that `com.sun.jmx.mbeanserver.Repository`/`JmxMBeanServer`
    // call on every `addMBean`/`queryNames` dispatch as soon as real JDK
    // bytecode constructs a genuine `JmxMBeanServer` (see
    // `native-api/src/registry.rs`'s `drop_synthetic_stubs` and
    // `vm/src/vm/vm_init.rs`). The real bytecode for these methods reads
    // private fields (`_ca_array`, `_compressed_storage`, ...) that this
    // synthetic 1-field `ObjectName` model never populates (construction is
    // always native-Bridge-shortcut, see `object_name_new`/`object_name_set_text`
    // above), so real bytecode's `_ca_array.length` NPEs. Implement these
    // against the same canonical-string text model the rest of this file
    // already uses (`object_name_parts`, `getDomain`, `getKeyProperty`)
    // instead.
    //
    // CORRECTED, lane H6 (2026-08-20): this used to add "`_ca_array` in
    // particular is a reference field, so an OUT-OF-BOUNDS read of it yields
    // `null`". That mechanism no longer exists. `try_alloc_concurrent_synthetic`
    // (`util_concurrent_ext.rs`) widens every allocation to
    // `max(real_instance_field_count, requested)`, so against a real
    // `java.management` image an `ObjectName` gets all FIVE slots here, not one
    // -- the read is in bounds and the slot is genuinely null. Same NPE, and it
    // matters which one it is: the old sentence also underwrites the P0 row's
    // claim that "any write past slot 0 is silently discarded", which is stale
    // for the same reason.
    r.register(
        cls,
        "getCanonicalKeyPropertyListString",
        "()Ljava/lang/String;",
        native_object_name_get_canonical_key_property_list_string, cratonvm_native_api::NativeKind::Bridge,
    );
    // RKC-ObjectName-03: `getSerializedNameString()` is a private helper
    // called from `writeObject()`'s non-compat branch (the default
    // `ObjectOutputStream.defaultWriteObject()` + explicit
    // `writeObject(getSerializedNameString())` path) to rebuild the
    // canonical name text from `_kp_array` via `writeKeyPropertyListString`.
    // The synthetic 1-field model never populates `_kp_array`, so real
    // bytecode NPEs on `_kp_array.length` the first time an ObjectName is
    // actually serialized — e.g. jmxmp's real remote `MBeanServerConnection`
    // wire protocol (`RemoteMBeanClientInterceptorTests`), never exercised
    // by the in-process `MBeanServer` path this synthetic model otherwise
    // covers. Derive the same output from the canonical text model instead,
    // mirroring `getCanonicalKeyPropertyListString` above.
    r.register(
        cls,
        "getSerializedNameString",
        "()Ljava/lang/String;",
        native_object_name_get_serialized_name_string, cratonvm_native_api::NativeKind::Bridge,
    );
    // H6-1 N5 follow-up: see `native_object_name_read_object`'s doc comment.
    r.register(
        cls,
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
        native_object_name_read_object, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cls, "isPattern", "()Z", native_object_name_is_pattern, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cls,
        "isDomainPattern",
        "()Z",
        native_object_name_is_domain_pattern, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "isPropertyPattern",
        "()Z",
        native_object_name_is_property_pattern, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "isPropertyListPattern",
        "()Z",
        native_object_name_is_property_list_pattern, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "apply",
        "(Ljavax/management/ObjectName;)Z",
        native_object_name_apply, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_object_name_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cls, "hashCode", "()I", native_object_name_hash_code, cratonvm_native_api::NativeKind::Bridge);
}

pub(in crate::jmx) fn object_name_string_arg(ctx: &dyn NativeContext, args: &[Value], index: usize) -> String {
    match args.get(index) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// The slot holding the canonical-name `String` on whichever
/// `javax.management.ObjectName` carrier this object actually is.
///
/// **JDK-ONLY-WAVE2 (the `JMX real path` P0 row, step 2: "convert").** These
/// three helpers used a hard-coded slot `0`. Against the REAL JDK 25 class that
/// is right only by coincidence -- `javap -p javax.management.ObjectName` on
/// 25.0.3+9 gives five instance fields in this order:
///
/// ```text
///   0  private transient java.lang.String                     _canonicalName
///   1  private transient javax.management.ObjectName$Property[] _kp_array
///   2  private transient javax.management.ObjectName$Property[] _ca_array
///   3  private transient java.util.Map<String,String>          _propertyList
///   4  private transient int                                   _compressed_storage
/// ```
///
/// so slot 0 happens to be `_canonicalName` today. A slot index against a real
/// layout is heap corruption rather than a wrong answer the moment that order
/// changes, and nothing in the tree was pinning it. Resolve by NAME instead.
///
/// The fallback to slot 0 is deliberate and is NOT a defaulting reader hiding a
/// wrong write: it fires only where the name does not resolve, i.e. the
/// synthetic single-slot carrier `object_name_new` fabricates when no real
/// `java.management` class is available. `set_field_by_name` is documented as a
/// **no-op when the field is not found**, so switching blindly to the by-name
/// setter would have silently dropped every write on that carrier.
pub(in crate::jmx) fn object_name_text_slot(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let cid = ctx.class_id_of_object(obj);
    ctx.resolve_field_index_by_class_id(cid, "_canonicalName")
        .unwrap_or(0)
}

pub(in crate::jmx) fn object_name_text(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    let slot = object_name_text_slot(ctx, obj);
    match ctx.get_field(obj, slot) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => ctx.read_string(obj).unwrap_or_default(),
    }
}

/// Store this file's text model into the canonical-name slot.
///
/// **READ THIS BEFORE TRYING TO POPULATE `_kp_array` / `_ca_array` /
/// `_propertyList` BY HAND (lane H6, 2026-08-20). That change is not merely
/// laborious, it is UNSOUND, and the reason is here rather than in the record
/// because this is the function that makes it unsound.**
///
/// The text this writes is the **source-order** name. The real JDK's
/// `_canonicalName` holds the **key-sorted** name. Measured on this host,
/// HotSpot 25.0.3+9, `new ObjectName("d:b=2,a=1,c=3")`:
///
/// ```text
///   getCanonicalName() = d:a=1,b=2,c=3    (== _canonicalName, ObjectName.java:1452)
///   toString()         = d:b=2,a=1,c=3    (source order, rebuilt from _kp_array)
/// ```
///
/// The two models therefore put **different strings in the same field**, and
/// every accessor in this file is written against the source-order one --
/// `native_object_name_get_key_property_list_string` is *correct only because*
/// of it.
///
/// That is what forbids populating the other three by hand:
/// `javax.management.ObjectName$Property` (`javap -p`) is
/// `{ int _key_index; int _key_length; int _value_length; }` with
/// `getKeyString(String)` / `getValueString(String)` taking the name string as
/// a PARAMETER. `_kp_array` and `_ca_array` are not independent data -- they
/// are an **index into `_canonicalName`**. Filling them while this slot holds
/// source-order text leaves the offsets pointing at the wrong characters, so
/// `getKeyProperty()` would return silently wrong substrings instead of the
/// NPE it returns today. A wrong answer that looks like an answer is worse
/// than the null.
///
/// The whole family moves together or none of it does. See
/// `docs/known-issues/jdk-only/H6-1-*` §2 for the migration and its
/// precondition.
///
/// Returns `obj` at its current address. gc-common w19-e: the name `String`'s
/// allocation can collect, and this stored it through the address from before
/// it -- and `object_name_new` then returned that address.
pub(in crate::jmx) fn object_name_set_text(ctx: &mut dyn NativeContext, obj: ObjectRef, text: String) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(obj);
    let s = scope.create_string(&text);
    let obj = scope.get(&obj_h);
    let slot = object_name_text_slot(&*scope, obj);
    scope.set_field(obj, slot, Value::Object(Some(s)));
    obj
}

pub(in crate::jmx) fn object_name_new(
    ctx: &mut dyn NativeContext,
    text: String,
) -> Result<ObjectRef, MethodCallFailed> {
    // Requested width stays 1: `try_alloc_concurrent_synthetic` already widens
    // the allocation to the loaded class's real instance-field count when one
    // exists (`max(real, requested)`), so a real `ObjectName` gets all five
    // slots here and only the synthetic carrier gets one. Padding the request
    // to 5 would pad the SYNTHETIC carrier too, which is the direction the
    // object-layout audit wants to unwind ("convert, verify, unpad, then
    // drop"), not extend.
    //
    // WHAT THIS DOES NOT FIX: `_kp_array`, `_ca_array` and `_propertyList` are
    // still left null on a real `ObjectName`. Real bytecode reading them NPEs
    // -- that is the `(b) Layout` half of the P0 row and it is unaddressed
    // here. Only the canonical-name slot is now name-resolved.
    //
    // SBR wave 1 (lane JMX): which is why nothing outside this family calls
    // it any more. Its only live caller is the `getInstance(String)` native,
    // and under `--real-jdk` that native is retired
    // (`RETIRED_SHADOW_SBR_JMX_TRIPLES`) together with every accessor that
    // reads this model, so an instance built here can only ever meet the
    // natives, never real bytecode. A new producer must call
    // `ObjectName.getInstance` through `invoke` instead (see
    // `native_platform_managed_object_name`).
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/management/ObjectName", 1)?;
    Ok(object_name_set_text(ctx, obj, text))
}

pub(in crate::jmx) fn register_object_instance(r: &mut NativeMethodRegistry) {
    // JDK-ONLY-WAVE2: pinned, same reason as `register_object_name` above.
    let cls = "javax/management/ObjectInstance";
    r.register(
        cls,
        "getObjectName",
        "()Ljavax/management/ObjectName;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field_by_name(this, "name")))
        },
    cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getClassName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "className")))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

pub(in crate::jmx) fn object_name_quote_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '*' => out.push_str("\\*"),
            '?' => out.push_str("\\?"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Snapshot String pairs from either the native HashMap-shaped Hashtable or a
/// real JDK Hashtable. Both store their buckets in field 0 and chain entries
/// through field 3; the entry's key/value slots differ only because a real
/// `Hashtable$Entry` has its hash in slot 0. Reading fields only means this
/// does not need native GC pins.
pub(in crate::jmx) fn object_name_table_pairs(ctx: &dyn NativeContext, table: ObjectRef) -> Vec<(String, String)> {
    let buckets = match ctx.get_field(table, 0) {
        Value::Object(Some(buckets)) => buckets,
        _ => return Vec::new(),
    };
    let mut pairs = Vec::new();
    for index in 0..ctx.array_length(buckets) {
        let mut entry = ctx.get_array_element(buckets, index);
        while let Value::Object(Some(node)) = entry {
            let real_entry = matches!(ctx.get_field(node, 0), Value::Int(_));
            let (key, value) = if real_entry {
                (ctx.get_field(node, 1), ctx.get_field(node, 2))
            } else {
                (ctx.get_field(node, 0), ctx.get_field(node, 1))
            };
            if let (Value::Object(Some(key)), Value::Object(Some(value))) = (key, value) {
                if let (Some(key), Some(value)) = (ctx.read_string(key), ctx.read_string(value)) {
                    pairs.push((key, value));
                }
            }
            entry = ctx.get_field(node, 3);
        }
    }
    pairs
}

pub(in crate::jmx) fn object_name_from_domain_table(
    ctx: &mut dyn NativeContext,
    domain: &str,
    table: Option<ObjectRef>,
) -> String {
    // ObjectName(String, Hashtable) accepts every property in the supplied
    // table. The previous name/type-only shortcut silently dropped arbitrary
    // Spring properties (including name1/name2, context and identity), which
    // became visible once getKeyPropertyList stopped falling through to an
    // NPE. Retain every non-empty String entry instead.
    let pairs = table
        .map(|table| object_name_table_pairs(ctx, table))
        .unwrap_or_default()
        .into_iter()
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        format!("{domain}:*")
    } else {
        format!("{domain}:{}", pairs.join(","))
    }
}

pub(in crate::jmx) fn native_object_name_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let text = object_name_string_arg(ctx, args, 1);
    if !object_name_has_required_structure(&text) {
        return Err(throw_malformed_object_name(ctx, &text));
    }
    object_name_set_text(ctx, this, text);
    Ok(None)
}

/// The native ObjectName text model deliberately avoids duplicating the JDK's
/// full parser, but constructors must still reject a string that cannot name an
/// MBean.  In particular, Spring first tries a bean key such as
/// `integrationMbeanExporter` as an ObjectName and relies on the real
/// `MalformedObjectNameException` to select its documented package-domain
/// fallback. Accepting that bare string makes the fallback unreachable and
/// corrupts MBeanServer.getDomains().
pub(in crate::jmx) fn object_name_has_required_structure(text: &str) -> bool {
    let Some((_domain, properties)) = text.split_once(':') else {
        return false;
    };
    if properties.is_empty() || properties == "*" {
        return !properties.is_empty();
    }
    properties.split(',').all(|property| {
        property == "*"
            || property
                .split_once('=')
                .is_some_and(|(key, _value)| !key.is_empty())
    })
}

pub(in crate::jmx) fn throw_malformed_object_name(ctx: &mut dyn NativeContext, text: &str) -> MethodCallFailed {
    let message = format!("Key properties cannot be empty: {text}");
    let detail = ctx.create_string(&message);
    match ctx.new_object_initialized(
        "javax/management/MalformedObjectNameException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        Ok(Some(Value::Object(Some(exception)))) => MethodCallFailed::ExceptionThrown(exception),
        _ => RuntimeError::IllegalArgumentException { message }.into(),
    }
}

pub(in crate::jmx) fn native_object_name_init_domain_key_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let domain = object_name_string_arg(ctx, args, 1);
    let key = object_name_string_arg(ctx, args, 2);
    let value = object_name_string_arg(ctx, args, 3);
    let text = if key.is_empty() {
        format!("{domain}:*")
    } else {
        format!("{domain}:{key}={value}")
    };
    object_name_set_text(ctx, this, text);
    Ok(None)
}

pub(in crate::jmx) fn native_object_name_init_domain_table(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let domain = object_name_string_arg(ctx, args, 1);
    let table = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let text = object_name_from_domain_table(ctx, &domain, table);
    object_name_set_text(ctx, this, text);
    Ok(None)
}

pub(in crate::jmx) fn native_object_name_get_instance_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = object_name_string_arg(ctx, args, 0);
    if !object_name_has_required_structure(&text) {
        return Err(throw_malformed_object_name(ctx, &text));
    }
    Ok(Some(Value::Object(Some(object_name_new(ctx, text)?))))
}

pub(in crate::jmx) fn native_object_name_get_instance_object(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

pub(in crate::jmx) fn native_object_name_quote(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let text = object_name_string_arg(ctx, args, 0);
    let quoted = ctx.create_string(&object_name_quote_text(&text));
    Ok(Some(Value::Object(Some(quoted))))
}

pub(in crate::jmx) fn native_object_name_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.create_string(&object_name_text(ctx, this));
    Ok(Some(Value::Object(Some(s))))
}

/// `ObjectName.getCanonicalName()` sorts key properties while retaining the
/// domain and property-list wildcard semantics.  The synthetic ObjectName
/// model keeps the original input text for `toString()`, but JMX identity is
/// defined by this canonical form rather than the input order.
pub(in crate::jmx) fn native_object_name_get_canonical_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let canonical = canonical_object_name_text(&object_name_text(ctx, this));
    Ok(Some(Value::Object(Some(ctx.create_string(&canonical)))))
}

pub(in crate::jmx) fn native_object_name_domain(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = object_name_text(ctx, this);
    let domain = text
        .split_once(':')
        .map(|(d, _)| d)
        .unwrap_or(text.as_str());
    let s = ctx.create_string(domain);
    Ok(Some(Value::Object(Some(s))))
}

pub(in crate::jmx) fn native_object_name_get_key_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = object_name_string_arg(ctx, args, 1);
    let text = object_name_text(ctx, this);
    for (property_key, value) in object_name_property_pairs(&text) {
        if property_key == key {
            // ObjectName returns the original property-value text,
            // including quote delimiters when the value was quoted.
            let s = ctx.create_string(value);
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    // Real JDK semantics: no such key property -> null (not an exception).
    Ok(Some(Value::Object(None)))
}

/// Construct a fresh Java map from ObjectName's text representation.  The
/// real implementation memoizes this privately; a fresh map is sufficient for
/// the synthetic model and prevents callers from observing shared mutable
/// state.  Every object held across allocating Java calls is pinned so this is
/// safe with the moving collector.
pub(in crate::jmx) fn object_name_key_property_map(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    map_class: &str,
) -> MethodCallResult {
    let map = match ctx.new_object_initialized(map_class, "()V", &[])? {
        Some(Value::Object(Some(map))) => map,
        other => return Ok(other),
    };
    let pin_base = ctx.pin_native_root(map);
    let text = object_name_text(ctx, this);

    for (key, value) in object_name_property_pairs(&text) {
        let key = ctx.create_string(key);
        let key_pin = ctx.pin_native_root(key);
        let value = ctx.create_string(value);
        let value_pin = ctx.pin_native_root(value);
        let map = ctx.read_native_pin(pin_base, map);
        let result = ctx.invoke(
            map_class,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(map)),
                Value::Object(Some(ctx.read_native_pin(key_pin, key))),
                Value::Object(Some(ctx.read_native_pin(value_pin, value))),
            ],
        );
        if let Err(error) = result {
            ctx.unpin_native_roots(pin_base);
            return Err(error);
        }
    }

    let map = ctx.read_native_pin(pin_base, map);
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Object(Some(map))))
}

pub(in crate::jmx) fn native_object_name_get_key_property_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(object_name))) => *object_name,
        _ => return Ok(Some(Value::Object(None))),
    };
    object_name_key_property_map(ctx, this, "java/util/HashMap")}

pub(in crate::jmx) fn native_object_name_get_key_property_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(object_name))) => *object_name,
        _ => return Ok(Some(Value::Object(None))),
    };
    object_name_key_property_map(ctx, this, "java/util/Hashtable")}

/// `ObjectName.getKeyPropertyListString()` returns the source-order property
/// list (unlike the canonical accessor, it does not sort keys) and omits a
/// trailing property-list wildcard.
pub(in crate::jmx) fn native_object_name_get_key_property_list_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(object_name))) => *object_name,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = object_name_text(ctx, this);
    let properties = text
        .split_once(':')
        .map(|(_, properties)| properties)
        .unwrap_or("");
    let list = split_object_name_properties(properties)
        .into_iter()
        .filter(|property| *property != "*")
        .collect::<Vec<_>>()
        .join(",");
    Ok(Some(Value::Object(Some(ctx.create_string(&list)))))
}

/// Simple JMX-style glob match:  = any run of characters,  = any
/// single character, everything else literal. No escaping (matches the
/// simplicity level of the rest of this synthetic ObjectName model).
pub(in crate::jmx) fn object_name_glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            Some(b'?') => !t.is_empty() && go(&p[1..], &t[1..]),
            Some(pc) => t.first() == Some(pc) && go(&p[1..], &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// Split a canonical ObjectName string into (domain, key=value pairs,
/// is_property_pattern). is_property_pattern is true for a trailing ",*"
/// or a bare "*" property list (JMX's "any additional properties allowed"
/// wildcard) -- as opposed to an exact/full property list, which must match
/// the candidate's property count exactly, not just be a subset.
pub(in crate::jmx) fn object_name_parts(text: &str) -> (String, Vec<(String, String)>, bool) {
    let (domain, props_str) = text.split_once(':').unwrap_or((text, ""));
    let is_pattern = props_str == "*" || props_str.ends_with(",*");
    let props = object_name_property_pairs(text)
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    (domain.to_string(), props, is_pattern)
}

/// Return the non-wildcard key/value pairs while preserving the exact value
/// text.  In particular, commas in quoted values are data, not separators.
pub(in crate::jmx) fn object_name_property_pairs(text: &str) -> Vec<(&str, &str)> {
    let properties = text
        .split_once(':')
        .map(|(_, properties)| properties)
        .unwrap_or("");
    split_object_name_properties(properties)
        .into_iter()
        .filter(|property| *property != "*")
        .filter_map(|property| property.split_once('='))
        .collect()
}

/// Split a key-property list on commas not protected by ObjectName quoting.
/// This deliberately preserves each token verbatim: canonicalisation changes
/// property *order*, never escaping or quoted values.
pub(in crate::jmx) fn split_object_name_properties(properties: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in properties.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == ',' && !quoted {
            parts.push(&properties[start..index]);
            start = index + ch.len_utf8();
        }
    }
    parts.push(&properties[start..]);
    parts
}

/// Return the canonical JMX identity text for an ObjectName.  ObjectName's
/// real canonical form sorts key-property names, so names differing only by
/// source order (for example `name=dataSource,type=HikariDataSource` versus
/// `type=HikariDataSource,name=dataSource`) compare equal and map to the same
/// MBean-server entry.
pub(in crate::jmx) fn canonical_object_name_text(text: &str) -> String {
    let (domain, properties) = match text.split_once(':') {
        Some(parts) => parts,
        None => return text.to_string(),
    };
    let mut properties = split_object_name_properties(properties);
    let property_list_pattern = matches!(properties.last(), Some(&"*"));
    if property_list_pattern {
        properties.pop();
    }
    properties.sort_unstable_by(|left, right| {
        let left_key = left.split_once('=').map(|(key, _)| key).unwrap_or(left);
        let right_key = right.split_once('=').map(|(key, _)| key).unwrap_or(right);
        left_key.cmp(right_key).then_with(|| left.cmp(right))
    });
    let mut canonical = String::with_capacity(text.len());
    canonical.push_str(domain);
    canonical.push(':');
    canonical.push_str(&properties.join(","));
    if property_list_pattern {
        if !properties.is_empty() {
            canonical.push(',');
        }
        canonical.push('*');
    }
    canonical
}

/// `ObjectName.getCanonicalKeyPropertyListString()`: the canonical
/// (domain-and-pattern-suffix-stripped) key-property-list portion of the
/// name, e.g. `"type=MBeanServerDelegate"` for
/// `"JMImplementation:type=MBeanServerDelegate"`. Mirrors real JDK's
/// `_canonicalName.substring(domainLength + 1, len)` but derived from the
/// synthetic text model (field 0) instead of the real private fields.
pub(in crate::jmx) fn native_object_name_get_canonical_key_property_list_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = object_name_text(ctx, this);
    let canonical = canonical_object_name_text(&text);
    let props_str = canonical.split_once(':').map(|(_, p)| p).unwrap_or("");
    let props_str = props_str.strip_suffix(",*").unwrap_or(props_str);
    let props_str = if props_str == "*" { "" } else { props_str };
    let s = ctx.create_string(props_str);
    Ok(Some(Value::Object(Some(s))))
}

/// `ObjectName.getSerializedNameString()`: see `RKC-ObjectName-03` at the
/// registration site.
///
/// **CORRECTED, lane H6 (2026-08-20). This comment used to read "the result is
/// simply the canonical name text (domain + sorted key properties)" and the
/// body returned `canonical_object_name_text(..)`. Both were wrong**, and the
/// oracle says so directly — HotSpot 25.0.3+9 on this host, `ONProbe`:
///
/// ```text
/// new ObjectName("d:b=2,a=1,c=3")
///   getCanonicalName()  = d:a=1,b=2,c=3     <- SORTED
///   toString()          = d:b=2,a=1,c=3     <- SOURCE ORDER
/// ```
///
/// and `ObjectName.java:1652` is `public String toString() { return
/// getSerializedNameString(); }`. So `getSerializedNameString()` is the
/// **source-order** text, not the canonical one: the real body walks `_kp_array`
/// (which is in source order) pulling substrings out of `_canonicalName`
/// (which is in canonical order) — that is the whole reason the two arrays
/// exist. Returning the canonical form here made every serialized `ObjectName`
/// on the jmxmp wire disagree with HotSpot on any name whose keys were not
/// already sorted, which is the exact path `RKC-ObjectName-03` was written for.
///
/// In this file's text model the source-order text is `object_name_text` itself,
/// so this is now the same answer `native_object_name_to_string` gives — which
/// is what the JDK's own one-line `toString()` asserts.
pub(in crate::jmx) fn native_object_name_get_serialized_name_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let text = object_name_text(ctx, this);
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

/// `readObject(ObjectInputStream)` -- found via `RJdkJmx`'s H6-1 N5
/// serialization-round-trip vector, which is the only vector in the corpus
/// that ever exercised this method.
///
/// `writeObject` needs no override: it is real, un-intercepted bytecode that
/// calls the already-intercepted `getSerializedNameString()` (this file's text
/// model, verbatim) and serializes that one `String`. `readObject` is real
/// bytecode too, but its OWN job -- `construct(s)` -- computes the REAL
/// key-SORTED string and writes it to `_canonicalName` with a genuine
/// `putfield`, because `construct` has no idea this file's convention inverts
/// that field's meaning. The result was a `_canonicalName` in the wrong order
/// for every accessor this file registers, since dispatch keys on the
/// receiver's CLASS (`H11-1`) and cannot tell a deserialized instance from one
/// `object_name_new` built: `toString()` on a round-tripped name silently
/// returned canonical order instead of the source order HotSpot preserves.
///
/// Fixed the same way `<init>(String)` is: skip `construct()` entirely and
/// re-run this file's own text-model write with the exact string `writeObject`
/// serialized, so a deserialized `ObjectName` ends up in the identical state a
/// freshly-constructed one would.
pub(in crate::jmx) fn native_object_name_read_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(None),
    };
    // gc-common w19-e: `readObject` is Java; the receiver is rooted across it
    // (the text used to be stored through the address from before the call).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let text = match scope.invoke_virtual(stream, "readObject", "()Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(s))) => scope.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let this = scope.get(&this_h);
    object_name_set_text(&mut *scope, this, text);
    Ok(None)
}

pub(in crate::jmx) fn native_object_name_is_domain_pattern(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let text = object_name_text(ctx, this);
    let (domain, _, _) = object_name_parts(&text);
    Ok(Some(Value::Int(
        (object_name_has_unquoted_wildcard(&domain)) as i32,
    )))
}

pub(in crate::jmx) fn native_object_name_is_property_list_pattern(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let text = object_name_text(ctx, this);
    let (_, _, is_plist_pattern) = object_name_parts(&text);
    Ok(Some(Value::Int(is_plist_pattern as i32)))
}

pub(in crate::jmx) fn native_object_name_is_property_pattern(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let text = object_name_text(ctx, this);
    let (_, props, is_plist_pattern) = object_name_parts(&text);
    let value_pattern = props
        .iter()
        .any(|(_, v)| object_name_has_unquoted_wildcard(v));
    Ok(Some(Value::Int((is_plist_pattern || value_pattern) as i32)))
}

/// `ObjectName.isPattern()`: true iff the domain contains a wildcard or the
/// name is a property pattern (property-list pattern, e.g. `"d:k=v,*"`, or a
/// property-value pattern, e.g. `"d:k=*"`).
pub(in crate::jmx) fn object_name_has_unquoted_wildcard(text: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quoted {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            continue;
        }
        if !quoted && matches!(ch, '*' | '?') {
            return true;
        }
    }
    false
}

pub(in crate::jmx) fn native_object_name_is_pattern(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let text = object_name_text(ctx, this);
    let (domain, props, is_plist_pattern) = object_name_parts(&text);
    let domain_pattern = object_name_has_unquoted_wildcard(&domain);
    let value_pattern = props
        .iter()
        .any(|(_, v)| object_name_has_unquoted_wildcard(v));
    Ok(Some(Value::Int(
        (domain_pattern || is_plist_pattern || value_pattern) as i32,
    )))
}

pub(in crate::jmx) fn native_object_name_apply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pattern = object_name_text(ctx, this);
    let candidate = object_name_text(ctx, target);

    let (p_domain, p_props, p_is_pattern) = object_name_parts(&pattern);
    let (c_domain, c_props, _) = object_name_parts(&candidate);

    // Domain: empty pattern domain ("" from a leading ":") means "any
    // domain"; otherwise exact match or, if the pattern domain itself
    // carries a wildcard, a glob match.
    let domain_ok = p_domain.is_empty()
        || p_domain == c_domain
        || ((p_domain.contains('*') || p_domain.contains('?'))
            && object_name_glob_match(&p_domain, &c_domain));

    // Properties: a pure property pattern ("domain:*", no explicit
    // key=value pairs) matches any property set once the domain matches.
    // Otherwise every pattern key must be present in the candidate with a
    // matching value (glob-matched if the pattern's value carries a
    // wildcard); a non-pattern (exact) property list additionally requires
    // the candidate to have exactly that many properties, not a superset.
    let props_ok = if p_props.is_empty() && p_is_pattern {
        true
    } else {
        let all_match = p_props.iter().all(|(pk, pv)| {
            c_props.iter().any(|(ck, cv)| {
                ck == pk
                    && (cv == pv
                        || ((pv.contains('*') || pv.contains('?'))
                            && object_name_glob_match(pv, cv)))
            })
        });
        all_match && (p_is_pattern || c_props.len() == p_props.len())
    };

    Ok(Some(Value::Int((domain_ok && props_ok) as i32)))
}

pub(in crate::jmx) fn native_object_name_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        (canonical_object_name_text(&object_name_text(ctx, this))
            == canonical_object_name_text(&object_name_text(ctx, other))) as i32,
    )))
}

pub(in crate::jmx) fn native_object_name_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut hash = 0i32;
    for b in canonical_object_name_text(&object_name_text(ctx, this)).bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(hash)))
}

/// Synthetic `ManagementFactory.getPlatformMBeanServer()` used in BOTH
/// real- and fake-JDK modes.
///
/// UPDATED 2026-07-14: this was previously tagged SyntheticStub on the
/// theory that real-JDK mode should "normally run the JDK bytecode for
/// this method" and only fall back to this stub for fake-JDK launches.
/// That theory was never actually exercised until `set_drop_synthetic_stubs`
/// started defaulting on for real-JDK mode (dev d8092acb) and this
/// registration got dropped for the first time: real bytecode for
/// `getPlatformMBeanServer()` -> `MBeanServerFactory.createMBeanServer()`
/// -> `new JmxMBeanServer(...)` -> ... -> `Repository.addMBean` ->
/// `ObjectName.getCanonicalKeyPropertyListString()` NPEs on a null
/// `_ca_array` deep inside real `com.sun.jmx.mbeanserver.*` bytecode that
/// this VM has apparently never successfully interpreted end-to-end before.
/// That's a real, deep, uninvestigated interpreter/real-mode gap -- not
/// something to chase here. Tag as Bridge (registered in both modes,
/// intercepting the real bytecode path entirely) so boot doesn't depend on
/// that untested path succeeding, matching this file's established
/// register_mbean_server/register_management_factory precedent ("its own
/// comment: `register_mbean_server` ... `register_management_factory` ...
/// deliberately called unconditionally by `register_jmx_natives` in BOTH
/// real- and synthetic-JDK registration branches").
pub fn register_management_factory_platform_server_stub(r: &mut NativeMethodRegistry) {
            r.register(
        "java/lang/management/ManagementFactory",
        "getPlatformMBeanServer",
        "()Ljavax/management/MBeanServer;",
        |ctx, _args| Ok(Some(Value::Object(Some(platform_mbean_server(ctx)?)))), cratonvm_native_api::NativeKind::Bridge,
    );
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

/// gci i12/ul (F1): HotSpot's cumulative loaded-class count
/// (`JMM_CLASS_LOADED_COUNT`, what `VMManagementImpl.getTotalClassCount`
/// answers): every class ever loaded, so the classes loaded now plus the ones
/// unloaded since. Monotonic across an unload, and `total - unloaded` is the
/// live count the JDK's `ClassLoadingImpl.getLoadedClassCount` derives from
/// it. Saturates rather than wrapping. Pure.
pub(crate) fn total_loaded_class_count(loaded: usize, unloaded: u64) -> i64 {
    let loaded = i64::try_from(loaded).unwrap_or(i64::MAX);
    let unloaded = i64::try_from(unloaded).unwrap_or(i64::MAX);
    loaded.saturating_add(unloaded)
}

/// gci i44/sp: one figure of [`NativeContext::safepoint_stats_ms`], `0` when
/// the VM keeps none; saturated into the JMM's `jlong`.
pub(in crate::jmx) fn safepoint_figure(ctx: &dyn NativeContext, pick: fn((u64, u64, u64, u64)) -> u64) -> i64 {
    safepoint_figure_of(ctx.safepoint_stats_ms(), pick)
}

/// [`safepoint_figure`]'s arithmetic, pure (unit-tested).
pub(in crate::jmx) fn safepoint_figure_of(
    stats: Option<(u64, u64, u64, u64)>,
    pick: fn((u64, u64, u64, u64)) -> u64,
) -> i64 {
    stats.map_or(0, |s| i64::try_from(pick(s)).unwrap_or(i64::MAX))
}

/// RKC16N.10: register the `sun.management.VMManagementImpl` natives so
/// `java.lang.management.ManagementFactory.<clinit>` doesn't throw
/// `UnsatisfiedLinkError` and JBoss Modules' `Module.<clinit>` chain
/// (Keycloak boot) can advance past it.
///
/// **Call from both real-JDK and synthetic-JDK registration paths.** This
/// function lives outside `register_jmx_natives` because that helper is only
/// reachable from `register_synthetic_overrides`, which is skipped in
/// real-JDK mode (synthetic-mode-only field layouts).
///
/// gci i45/rv: this doc block is the function's again; i44/sp had inserted the
/// two `safepoint_figure` helpers between it and the `fn`, so rustdoc gave this
/// text to `safepoint_figure` and left `register_vm_management_impl` undocumented.
pub fn register_vm_management_impl(r: &mut NativeMethodRegistry) {
    // gc-common w8-f: latch the `RuntimeMXBean.getStartTime()` /
    // `getUptime()` / `GcInfo` time origin HERE, at VM init (every real-JDK
    // registration path calls this function, before any Java runs), instead
    // of at the first JMX call. It used to be latched lazily, so `getUptime()`
    // counted from the first time anything ASKED -- a boot-time logger (Spring
    // Boot's "process running for") read ~0 -- and `GcInfo.getStartTime()` of
    // every collection before that first question read 0.
    //
    // gc-common w17-e: this is now only the FALLBACK origin, for a context
    // whose `NativeContext::vm_start_instant` answers `None`. A VM that
    // reports its own start instant is timed from it (`vm_clock`), so a
    // second VM in the process no longer reports the first VM's start.
    let _ = process_start_epoch_ms();
        // These are native entry points of real JDK classes, not replacements for
    // synthetic class bytecode. In real-JDK mode a SyntheticStub registration
    // is deliberately excluded from dispatch, which made getVersion0 appear
    // missing despite being registered here. Keep this bridge category pinned
    // so ManagementFactory's VMManagementImpl can link during application boot.
        let cls = "sun/management/VMManagementImpl";

    // Management interface version. OpenJDK reports "10.0" for JDK 8+.
    // Format: "<major>.<minor>" — JBoss / Hotspot consumers parse only major.
    r.register(
        cls,
        "getVersion0",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("10.0");
            Ok(Some(Value::Object(Some(s))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // JVM init-done time, epoch millis. Mirrors RuntimeMXBean.getStartTime.
    r.register(
        cls,
        "getStartupTime",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(vm_start_epoch_ms(&*ctx) as i64))),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // Process id. Same plumbing as RuntimeMXBean.getName which embeds PID.
    r.register(
        cls,
        "getProcessId",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(std::process::id() as i32))),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // REAL: seed the ten static `boolean` support fields the JDK's own
    // `VMManagementImpl` declares, from the same sources the `is*Supported()`
    // natives below answer from.
    //
    // The previous no-op was defensible only because every reader of those
    // fields (`isCompilationTimeMonitoringSupported()` and its nine siblings)
    // is itself shadowed by a native registered below — a property that held by
    // coincidence and that the next person to drop one entry from that batch
    // would have broken silently, leaving the surviving bytecode getter reading
    // an un-seeded `false`. Writing the fields removes the coupling. The values
    // stay in lock-step with the batch below by construction.
    //
    // `set_static_field_by_name` resolves class + field by name and no-ops when
    // either is absent, so this is inert in synthetic-JDK mode (where there is
    // no real `VMManagementImpl`) and cannot fail during `<clinit>`.
    r.register(
        cls,
        "initOptionalSupportFields",
        "()V",
        |ctx, _args| {
            let current_cpu = i32::from(current_thread_cpu_time_ns().is_some());
            let boot_cp = i32::from(boot_class_path(ctx).is_some());
            // Every non-constant below is the SAME expression its `is*Supported()`
            // native uses, evaluated once here — that is what keeps the seeded field
            // and the native from disagreeing.
            let other_cpu = i32::from(arbitrary_thread_cpu_time_supported(ctx));
            let comp_time = i32::from(ctx.jit_total_compile_time_ms().is_some());
            let synchronizer = i32::from(synchronizer_usage_supported());
            let thread_alloc = i32::from(ctx.current_thread_allocated_bytes().is_some());
            for (field, supported) in [
                ("compTimeMonitoringSupport", comp_time),
                // `ThreadJmxSnapshot::blocked_time_ms`/`waited_time_ms` are real,
                // filled by `vm_exec.rs` from the registry — the premise this
                // used to be seeded `0` under ("nothing in the VM records
                // blocked/waited durations") expired; see the matching fix
                // below and in `register_thread_mxbean_for`.
                ("threadContentionMonitoringSupport", 1),
                ("currentThreadCpuTimeSupport", current_cpu),
                ("otherThreadCpuTimeSupport", other_cpu),
                ("bootClassPathSupport", boot_cp),
                // Every Java-reachable monitor acquisition publishes its owner
                // (see `isObjectMonitorUsageSupported` below). A `0` here made
                // `ThreadImpl.verifyDumpThreads` throw on the 4-argument
                // `getThreadInfo` and on `dumpAllThreads(true, _)` while the
                // 3-argument overload answered from the same data.
                ("objectMonitorUsageSupport", 1),
                ("synchronizerUsageSupport", synchronizer),
                ("threadAllocatedMemorySupport", thread_alloc),
                // gci i14/mp finding 11: HotSpot's `libmanagement` seeds this
                // `true` (JMM >= 1.2.1). JDK 25's `isGcNotificationSupported()`
                // no longer reads it (it probes for
                // `com.sun.management.GarbageCollectorMXBean`), so only a
                // reflective read sees the seed; it is HotSpot's now.
                ("gcNotificationSupport", 1),
                // 0 by default (finding 11, narrowed): HotSpot answers 1, and `1`
                // makes `ManagementFactoryHelper.getDiagnosticCommandMBean()`
                // build the `com.sun.management:type=DiagnosticCommand` bean
                // that EVERY `getPlatformMBeanServer()` registers and asks for
                // its `MBeanInfo`. gci i28/dg: its four natives are registered
                // now (`register_diagnostic_command_impl`), so
                // `CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN` (default ON since
                // 2026-10-17, `=0` kills) seeds HotSpot's 1.
                (
                    "remoteDiagnosticCommandsSupport",
                    i32::from(jmx_diagnostic_command_mbean_enabled()),
                ),
            ] {
                ctx.set_static_field_by_name(
                    "sun/management/VMManagementImpl",
                    field,
                    Value::Int(supported),
                );
            }
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // The JVM options (`VmConfig::jvm_args`, via `jvm_input_arguments`).
    r.register(
        cls,
        "getVmArguments0",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Interpreter round i4 wave 85, lane L3: HotSpot answers
            // `Arguments::jvm_args_array()` (`jmm_GetInputArgumentArray`), the
            // options the launcher recorded (`VmConfig::jvm_args`), so
            // `RuntimeMXBean.getInputArguments()` lists `-Xmx64m`, `-Dfoo=bar`,
            // ... It answered an empty array (`L3W85InputArguments`).
            let args = ctx.jvm_input_arguments();
            // `String[]`, the declared type (i7-L2).
            let arr = cratonvm_native_builtins::t27_tls::materialize_java_string_array(ctx, &args);
            Ok(Some(Value::Object(Some(arr))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // -- VMManagementImpl `is*Supported` / `is*Enabled` queries --
    //
    // This is the REAL-JDK route for every flag the synthetic `*MXBean`
    // interfaces answer directly: `sun.management.ThreadImpl`,
    // `CompilationImpl` etc. hold a `VMManagement jvm` and delegate
    // (`ThreadImpl.isThreadCpuTimeSupported()` is literally
    // `jvm.isOtherThreadCpuTimeSupported()`; `CompilationImpl
    // .isCompilationTimeMonitoringSupported()` is
    // `jvm.isCompilationTimeMonitoringSupported()`), so anything fixed on the
    // interface side has to be fixed here too or the two surfaces disagree
    // about the same VM.
    //
    // `false`/0 is still the TRUTH for every name left in this batch: CratonVM
    // does not implement the optional JMM feature behind it, and `false` is
    // exactly what the spec wants for an unsupported feature — it also keeps the
    // corresponding `get*` natives below honest. Done as a batch; the consumer
    // iterates a static const list at clinit time.
    //
    // `isObjectMonitorUsageSupported` left this batch on 2026-09-24; see its
    // registration below.
    let false_zero: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Int(0))); // false / 0
                                               // `isThreadContentionMonitoringSupported`/`Enabled` used to live in this
                                               // false batch on the premise that nothing in the VM recorded blocked/
                                               // waited durations. That premise expired: `ThreadJmxSnapshot
                                               // ::blocked_time_ms`/`waited_time_ms` are real (see
                                               // `register_thread_mxbean_for`, which already answered `true` on the
                                               // `ThreadMXBean` interface directly — this is the OTHER route to the same
                                               // answer, the one `sun.management.ThreadImpl.isThreadContentionMonitoring
                                               // Supported()`'s real bytecode actually takes: `this.jvm
                                               // .isThreadContentionMonitoringSupported()`, where `jvm` is this
                                               // `VMManagementImpl`). Leaving this one `false` made the two surfaces
                                               // disagree — `ManagementFactory.getThreadMXBean()` (routes through here)
                                               // reported unsupported while `ThreadMXBean` classes registered directly
                                               // reported supported — and every caller in practice reaches the JDK
                                               // object, not the raw registered interface, so this is the answer that
                                               // was actually observed: Tomcat's `Diagnostics.getVMInfo()` (manager
                                               // webapp's `/status` page — `TestManagerWebapp.testServlets`) calls
                                               // `isThreadContentionMonitoringEnabled()` unconditionally and got
                                               // `UnsupportedOperationException` instead of HotSpot's `false`.
    r.register(
        cls,
        "isThreadContentionMonitoringEnabled",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(jmm_switch(ctx, |s| {
                s.thread_contention_monitoring
            })))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "isThreadContentionMonitoringSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::Bridge,
    );
    // gci i28/dg: in lock-step with the `remoteDiagnosticCommandsSupport`
    // seed in `initOptionalSupportFields` (the JDK's bytecode getter reads the
    // field; this native is its fallback): the default-ON
    // `CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN` answers HotSpot's `true`.
    r.register(
        cls,
        "isRemoteDiagnosticCommandsSupported",
        "()Z",
        |_ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                jmx_diagnostic_command_mbean_enabled(),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    {
        let name = "isGcNotificationSupported";
        r.register(cls, name, "()Z", false_zero, cratonvm_native_api::NativeKind::Bridge);
    }
    // REAL: TRUE, in lock-step with the `objectMonitorUsageSupport` seed in
    // `initOptionalSupportFields` (the field is what the JDK's bytecode getter
    // actually reads; this native is shadowed by it) and with the synthetic
    // `ThreadMXBean` surface in `register_thread_mxbean_for`. The `false` this
    // used to answer was wrong in every mode: every Java-reachable
    // `monitorenter` publishes its owner (`monitor_enter_blocking` and
    // `monitor_enter_synchronized_method` on both arms, `invoke_fast`'s
    // synchronized door, the JIT helpers, and the deopt / exception-dispatch
    // relocks), and the 3-argument `ThreadImpl.getThreadInfo` already reported
    // those monitors, while the 4-argument overload and
    // `dumpAllThreads(true, _)` threw `UnsupportedOperationException`.
    // Known fidelity gaps (not wrong answers about support): every
    // `MonitorInfo` is attributed to the innermost frame, and JNI-held
    // monitors are absent. See
    // `docs/feature-designs/native-dispatch-architecture.md`.
    r.register(
        cls,
        "isObjectMonitorUsageSupported",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // Thread allocated-memory accounting IS kept (a TLAB cursor), for the calling
    // thread: report it, so real `ThreadImpl.getThreadAllocatedBytes` stops
    // throwing `UnsupportedOperationException`. See `register_thread_impl`.
    r.register(
        cls,
        "isThreadAllocatedMemorySupported",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                ctx.current_thread_allocated_bytes().is_some(),
            ))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "isThreadAllocatedMemoryEnabled",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                jmm_switch(ctx, |s| s.thread_allocated_memory)
                    && ctx.current_thread_allocated_bytes().is_some(),
            ))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL (the four below), each in lock-step with its interface-side twin.
    //
    // `isThreadCpuTimeSupported` and `isOtherThreadCpuTimeSupported` are the
    // same question on this surface — the JDK's `ThreadImpl` routes its own
    // `isThreadCpuTimeSupported()` to `jvm.isOtherThreadCpuTimeSupported()` —
    // so both read the one probe, which exercises the arbitrary-thread route
    // end to end (see `management::arbitrary_thread_cpu_time_supported`).
    let other_thread_cpu_time: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                arbitrary_thread_cpu_time_supported(ctx),
            ))))
        };
    r.register(
        cls,
        "isOtherThreadCpuTimeSupported",
        "()Z",
        other_thread_cpu_time, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cls, "isSynchronizerUsageSupported", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(i32::from(synchronizer_usage_supported()))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cls,
        "isCompilationTimeMonitoringSupported",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                ctx.jit_total_compile_time_ms().is_some(),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: `isBootClassPathSupported` left the batch above. It is not an
    // optional-feature question the VM has no answer to — it asks whether the
    // VM established a boot class path at all, and `sun.boot.class.path` is
    // where the VM publishes one. Read that, in lock-step with the
    // `RuntimeMXBean` pair in `register_runtime_mxbean` and with the field
    // `initOptionalSupportFields` above now seeds.
    r.register(cls, "isBootClassPathSupported", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(i32::from(boot_class_path(ctx).is_some()))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // REAL: `isCurrentThreadCpuTimeSupported` / `isThreadCpuTimeEnabled` left
    // the batch above. Unlike arbitrary-thread CPU time (which needs an OS
    // handle for a thread we are not running on, hence the `false` retained
    // for `isThreadCpuTimeSupported`/`isOtherThreadCpuTimeSupported`), the
    // CALLING thread's CPU time is readable straight from the OS scheduler —
    // so `false` here was not a measurement, it was an unchecked claim. Answer
    // from whether the platform clock actually reads, so the flag can never
    // disagree with the `getCurrentThreadCpuTime` numbers on the ThreadMXBean
    // surface. Kept in lock-step with `register_thread_mxbean` below.
    let cpu_time_available: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                current_thread_cpu_time_ns().is_some(),
            ))))
        };
    r.register(
        cls,
        "isCurrentThreadCpuTimeSupported",
        "()Z",
        cpu_time_available, cratonvm_native_api::NativeKind::Bridge,
    );
    // Interpreter round i4 wave 84, lane L6: ENABLED is support AND the per-VM
    // switch `ThreadImpl.setThreadCpuTimeEnabled0` writes (HotSpot's
    // `jmm_GetBoolAttribute(THREAD_CPU_TIME)` reads its flag); it answered
    // support alone, so a disable never showed.
    r.register(
        cls,
        "isThreadCpuTimeEnabled",
        "()Z",
        |ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                current_thread_cpu_time_ns().is_some() && jmm_switch(ctx, |s| s.thread_cpu_time),
            ))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // `getVerboseClass` / `getVerboseGC` used to sit in the batch above, which
    // made them constants — but they are NOT feature-support queries, they are
    // the read side of a documented round-trip. The real
    // `ClassLoadingImpl.isVerbose()` / `MemoryImpl.isVerbose()` bytecode is
    // literally `return jvm.getVerboseClass()` / `return jvm.getVerboseGC()`,
    // so a constant here silently discarded every `setVerbose(true)` a JMX
    // client made. Read the per-VM flags the setters now write.
    r.register(
        cls,
        "getVerboseClass",
        "()Z",
        |ctx, _args| {
            let on = jmm_switch(ctx, |s| s.verbose_class);
            Ok(Some(Value::Int(i32::from(on))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getVerboseGC",
        "()Z",
        |ctx, _args| Ok(Some(Value::Int(i32::from(verbose_gc_get(&*ctx))))),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // -- VMManagementImpl long-typed counters / timers --
    //
    // These split into two groups:
    //   (a) metrics the VM genuinely does NOT track (compile time, per-phase
    //       class-load/verify/init timers, method-data size, safepoint
    //       timers, class byte sizes). The real JDK derives these from
    //       HotSpot PerfData counters we don't maintain. We have no real
    //       source, so they stay 0 — honest "not measured", and JBoss only
    //       surfaces them as diagnostic output, never control flow. They are
    //       FLAGGED here rather than silently faked.
    //   (b) metrics we DO have a real source for (cumulative class count,
    //       cumulative started-thread count) — wired below to the same VM
    //       accessors the already-correct Bridge MXBeans use
    //       (`loaded_class_count`, `active_thread_count`).
    //
    // Descriptor note: all of these are `long` (`()J`); live/peak/daemon
    // thread counts are `int` (see int-typed batch below). Mismatching the
    // descriptor makes the dispatcher miss the registration → ULE.
    let zero_long: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Long(0)));
    // (a) FLAGGED-0: no real VM source for any of these. Leaving them at 0
    // (not a fabricated non-zero) keeps diagnostics honest. If/when the VM
    // gains JIT-time / safepoint / per-class-phase accounting, wire here.
    for name in [
        "getLoadedClassSize",                // no per-class byte-size tracking
        "getUnloadedClassSize",              // no per-class byte-size tracking
        "getClassLoadingTime",               // no class-load timer
        "getMethodDataSize",                 // no profiling method-data area
        "getInitializedClassCount",          // not tracked separately from loaded
        "getClassInitializationTime",        // no clinit timer
        "getClassVerificationTime",          // no verify timer
    ] {
        r.register(cls, name, "()J", zero_long, cratonvm_native_api::NativeKind::Bridge);
    }
    // gci i44/sp: REAL. The GC barrier keeps HotSpot's four safepoint figures
    // (`NativeContext::safepoint_stats_ms`; `RuntimeService`'s
    // `sun.rt.safepoints` / `safepointTime` / `safepointSyncTime` /
    // `applicationTime`); 0 when the VM keeps none or under
    // `CRATONVM_JMX_SAFEPOINT_COUNTERS=0`.
    r.register(
        cls,
        "getSafepointCount",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(safepoint_figure(&*ctx, |s| s.0)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getTotalSafepointTime",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(safepoint_figure(&*ctx, |s| s.1)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getSafepointSyncTime",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(safepoint_figure(&*ctx, |s| s.2)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getTotalApplicationNonStoppedTime",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(safepoint_figure(&*ctx, |s| s.3)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // (b) REAL: `getTotalCompileTime` left the FLAGGED-0 batch above — "no JIT
    // compile-time accounting" was false. This is what the real
    // `CompilationImpl.getTotalCompilationTime()` calls once
    // `isCompilationTimeMonitoringSupported()` says yes, so it MUST be driven by
    // the same accessor that answers that flag, or the real-JDK bean promises a
    // number and then reports 0. Already in milliseconds, the JMX spec's unit.
    r.register(
        cls,
        "getTotalCompileTime",
        "()J",
        |ctx, _args| {
            Ok(Some(Value::Long(
                ctx.jit_total_compile_time_ms()
                    .map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX)),
            )))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // (b) REAL: cumulative count of classes the VM has loaded, as HotSpot's
    // `JMM_CLASS_LOADED_COUNT` (a monotonic counter): the live classes plus the
    // unloaded ones ([`total_loaded_class_count`]), the same sum the
    // `ClassLoadingMXBean.getTotalLoadedClassCount` registrations answer.
    // gci i12/ul (F1): this answered the LIVE count alone, and the JDK's
    // `sun.management.ClassLoadingImpl` derives `getLoadedClassCount()` as
    // `getTotalClassCount() - getUnloadedClassCount()`, so after any unload
    // the bean's total FELL and its loaded count was short by the unloaded
    // count.
    r.register(
        cls,
        "getTotalClassCount",
        "()J",
        |ctx, _args| {
            Ok(Some(Value::Long(total_loaded_class_count(
                ctx.loaded_class_count(),
                ctx.unloaded_class_count(),
            ))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // (b) REAL: classes reclaimed by class-loader unloading. This was in the
    // FLAGGED-0 batch above under "we never unload classes", but the VM does
    // track it — `unloaded_class_count()` is the same accessor
    // `ClassLoadingMXBean.getUnloadedClassCount` already reads — so the 0 was
    // stale, not honest.
    r.register(
        cls,
        "getUnloadedClassCount",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(ctx.unloaded_class_count() as i64))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // (b) REAL: cumulative started-thread count. Interpreter round i5 wave
    // 100, lane L6: the VM now counts platform-thread starts
    // (`ThreadRegistry::note_jmm_thread_start`, HotSpot's
    // `_total_threads_count`); this answered the live count, which falls as
    // threads exit and counted virtual threads.
    r.register(
        cls,
        "getTotalThreadCount",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(total_started_thread_count(&*ctx)))),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // -- VMManagementImpl int-typed counters --
    //
    // RKC16N.12: live/peak/daemon thread counts are declared `int`, not
    // `long`, in JDK 25's VMManagementImpl. Registering them with `()J`
    // (as the previous batch did) caused the dispatcher to never match
    // the call site, surfacing as `UnsatisfiedLinkError` during
    // `ManagementFactoryHelper.<clinit>` -> `new VMManagementImpl()`
    // chain on Keycloak boot.
    // REAL: live thread count — same `active_thread_count()` source as
    // ThreadMXBean.getThreadCount.
    r.register(
        cls,
        "getLiveThreadCount",
        "()I",
        // Wave 100 (lane L6): live PLATFORM threads, as HotSpot counts
        // `JavaThread`s; a JDK virtual thread's registry entry is not one.
        |ctx, _args| Ok(Some(Value::Int(live_platform_thread_count(&*ctx)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: a genuine high-water mark now (`peak_thread_count`). Reporting the
    // CURRENT live count as the peak, as this used to, is not merely imprecise
    // — a peak that FALLS when threads exit is not a peak, and it left
    // `resetPeakThreadCount` below with no state to reset.
    r.register(
        cls,
        "getPeakThreadCount",
        "()I",
        |ctx, _args| Ok(Some(Value::Int(peak_thread_count(ctx)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: daemon-thread count. The previous FLAGGED-0 ("not tracked
    // separately by the VM") was wrong — the flag IS carried, on
    // `Thread.holder.daemon`, which is exactly where the VM's own shutdown
    // logic reads it (`vm_exec.rs::read_thread_daemon_flag`). Counting the
    // live threads that carry it is a measurement, not a fabricated split.
    r.register(
        cls,
        "getDaemonThreadCount",
        "()I",
        |ctx, _args| Ok(Some(Value::Int(daemon_thread_count(ctx)))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: "resets the peak thread count to the current number of live
    // threads" (JMM). There IS peak state to reset now — the old comment's
    // premise was the missing high-water mark, not a spec exemption.

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
    r.register(
        cls,
        "getUptime0",
        "()J",
        |ctx, _args| Ok(Some(Value::Long(uptime_ms(&*ctx) as i64))),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getAvailableProcessors",
        "()I",
        |ctx, _args| {
            // Container-aware (cgroup CPU quota under -XX:+UseContainerSupport),
            // matching what Runtime.availableProcessors() reports.
            Ok(Some(Value::Int(ctx.available_processor_count())))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

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
            // gen r4w2/obs: the collector's own pools when it describes them
            // (generational: `Eden Space`, `Survivor Space`, `Tenured Gen`).
            //
            // The historical pair otherwise — same array class, same two beans,
            // but built through the rooted helper: the array used to be held
            // unrooted across both bean allocations.
            let mut names: Vec<&'static str> =
                ctx.gc_memory_pools().iter().map(|p| p.name).collect();
            // zs s3/ob2, zt t1/ob: a collector that describes its heap pools
            // gets a non-heap list naming `Metaspace` -- gci i15/nh's five
            // HotSpot pools, or the VM's own modelled set where it states one
            // (ZGC under `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0`, see
            // `pool_names_with_non_heap`). Decided without measuring any
            // non-heap pool (the VM's measure may walk the class store).
            let described = !names.is_empty();
            if names.is_empty() {
                names = vec!["Eden Space", "Old Gen"];
            } else {
                // gci i15/nh: HotSpot's five non-heap pools first (see
                // "Non-heap pools and managers").
                names = pool_names_with_non_heap(&*ctx, &names);
            }
            let arr = alloc_pool_bean_array(ctx, &names)?;
            if described {
                // zs s3/ob2: from now on ZGC's collection edges sample the
                // class metadata for the `GcInfo`s' `Metaspace` key (a no-op
                // off ZGC and under `CRATONVM_ZGC_JMX_METASPACE_POOL=0`).
                ctx.note_non_heap_pools_published();            }
            Ok(Some(Value::Object(Some(arr))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        memory_impl,
        "getMemoryManagers0",
        "()[Ljava/lang/management/MemoryManagerMXBean;",
        |ctx, _args| {
            // gen r4w2/obs: one bean per collector when the collector describes
            // them (generational: `Copy`, `MarkSweepCompact`).
            //
            // gen r4w3/obs2: built as HotSpot builds them —
            // `GarbageCollectorExtImpl.createGarbageCollector` — so they are
            // `com.sun.management.GarbageCollectorMXBean`s that can carry
            // `getLastGcInfo()` and GC notifications. `MemoryImpl` caches this
            // array in its `mgrs` static; that cache is what every other route
            // hands out and what the notification drain delivers to.
            //
            // gci i15/nh: preceded by the two non-heap managers
            // (`CodeCacheManager`, `Metaspace Manager`), plain
            // `MemoryManagerImpl`s, as HotSpot's `_managers_list`.
            let names: Vec<&'static str> = ctx.gc_collectors().iter().map(|c| c.name).collect();
            if !names.is_empty() {
                let arr = alloc_manager_bean_array(ctx, &names)?;
                return Ok(Some(Value::Object(Some(arr))));
            }
            let mgr_cid = ctx
                .ensure_class_initialized("sun/management/MemoryManagerImpl")
                .unwrap_or(ClassId::new(0));
            let mut scope = NativeHandleScope::new(ctx);
            let arr_obj = scope.new_ref_array(mgr_cid, 1);
            let arr_h = scope.root(arr_obj);
            // GarbageCollectorImpl extends MemoryManagerImpl + implements
            // GarbageCollectorMXBean — single instance covers both the
            // manager list and (via the instanceof filter) the GC list.
            // Building it allocates, so the array's address is re-read after.
            let gc = alloc_garbage_collector_impl(&mut *scope, "G1 Young Generation")?;
            let arr = scope.get(&arr_h);
            scope.set_array_element(arr, 0, Value::Object(Some(gc)));
            Ok(Some(Value::Object(Some(arr))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // setVerboseGC(boolean) — the write side of the `-verbose:gc` round-trip.
    // Accepting and ignoring made `MemoryMXBean.setVerbose(b)` unobservable
    // through `isVerbose()`, which the JMM contract requires.
    r.register(
        memory_impl,
        "setVerboseGC",
        "(Z)V",
        |ctx, args| {
            verbose_gc_set(&*ctx, bool_flag_arg(args));
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // isVerbose()Z — `alloc_memory_mxbean` allocates this bean as a real
    // `sun/management/MemoryImpl` (not a purely-synthetic interface stamp,
    // unlike e.g. `ClassLoadingMXBean`), so the real-JDK bytecode
    // `MemoryImpl.isVerbose() { return jvm.getVerboseGC(); }` is reachable
    // by normal virtual dispatch and wins over the sibling native already
    // registered on the `MemoryMXBean` *interface* (interface natives never
    // shadow a concrete class's own bytecode). Since our `MemoryImpl`
    // instances are built via `alloc_concurrent_synthetic` rather than the
    // real `<init>(VMManagement)` constructor, the `jvm` field is never
    // populated and stays null, so that bytecode NPEs
    // ("Cannot invoke sun.management.VMManagement.getVerboseGC() because
    // this.jvm is null") the moment anything calls `isVerbose()` — Tomcat's
    // manager webapp status page does, via `MemoryMXBean.isVerbose()`
    // (TestManagerWebapp.testServlets: expected 200 got 500). Register
    // directly on the concrete class so it wins dispatch — and answer from
    // the same per-VM flag `setVerboseGC` writes, which is what the
    // NPE'ing bytecode (`return jvm.getVerboseGC()`) would have produced.
    // The previous hard-coded 0 made the setter's write invisible.
    r.register(memory_impl, "isVerbose", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(i32::from(verbose_gc_get(&*ctx)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getMemoryUsage0(boolean heap) — for the HEAP case we have REAL sources
    // for every field: `initial_heap_bytes()`/`heap_allocated_bytes()` (the
    // same accessors `Runtime.maxMemory()`/heap `MemoryUsage` already use)
    // and `max_heap_bytes()` (the configured `-Xmx`). Reporting `init` as 0
    // was an oversight (Spring Boot's `ProcessInfoTests.memoryInfoIsAvailable`
    // asserts it `isPositive()`, matching every real JVM's non-zero `-Xms`).
    //
    // gci i15/nh: the non-heap case is now the sum of the five non-heap
    // pools (`non_heap_memory_usage`; Metaspace = the class metadata below,
    // the non-profiled code heap = the JIT's mapped code). The history:
    //
    // For the NON-HEAP case CratonVM has no per-pool (Metaspace/CodeCache)
    // accounting, but reporting the untracked JMM "unavailable" sentinel
    // (-1) for init/used/committed doesn't match any real JVM either — every
    // HotSpot process has *some* non-heap footprint from the moment any
    // bytecode runs, and Spring Boot's `ProcessInfoTests` asserts all three
    // are positive. `used` is the VM's MEASURED class-metadata lower bound
    // (`NativeSystemAccess::class_metadata_bytes`, the heap summary's
    // `metaspace_used`: it grows when a class is defined and falls when its
    // loader is unloaded); only a context with no class store (mocks) falls
    // back to `loaded_class_count() × 4 KiB` (gc-common w3-f, 2026-09-23 —
    // this was the class-count estimate for every VM, see
    // `common-d-metaspace-numbers-are-fabricated-FIXED-20260923.md`). `max` stays -1: real
    // JVMs report the non-heap aggregate max as undefined unless
    // `-XX:MaxMetaspaceSize` is set — an honest sentinel, not a fake number.
    // CratonVM enforces that flag since gc-common w6-f, but HotSpot's
    // aggregate `max` with it set is the SUM of every non-heap pool's max
    // (Metaspace, Compressed Class Space, the code heaps), pools CratonVM does
    // not model; reporting the metaspace limit alone as the aggregate would be
    // a number no HotSpot prints, so `max` stays -1.
    r.register(
        memory_impl,
        "getMemoryUsage0",
        "(Z)Ljava/lang/management/MemoryUsage;",
        |ctx, args| {
            let is_heap = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
            let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4)?;
            // Slots by NAME -- see `memory_usage_slots`.
            let [s_init, s_used, s_committed, s_max] = memory_usage_slots(ctx, obj);
            let [init, used, committed, max] = if is_heap {
                heap_memory_usage(ctx)
            } else {
                non_heap_memory_usage(ctx)
            };
            ctx.set_field(obj, s_init, Value::Long(init));
            ctx.set_field(obj, s_used, Value::Long(used));
            ctx.set_field(obj, s_committed, Value::Long(committed));
            ctx.set_field(obj, s_max, Value::Long(max));
            Ok(Some(Value::Object(Some(obj))))
        },
        cratonvm_native_api::NativeKind::Bridge,
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
    // KEEP the `loadNativeLib()V` no-op below: it is the whole fix, and it is
    // exact rather than approximate. `loadNativeLib` is a private static helper
    // used only by `ManagementFactory`, its entire body is
    // `System.loadLibrary("management")`, and CratonVM ships the JMM natives
    // in-process through `NativeMethodRegistry` — so "libmanagement is already
    // loaded" IS the post-condition, and returning normally states it.
    //
    // REMOVED (2026-07-28 stub sweep) the four `System.loadLibrary` /
    // `System.load` / `Runtime.loadLibrary0` / `Runtime.load0` registrations
    // that used to sit here as "belt-and-suspenders" (the two `load*` ones were
    // duplicate bodies, the two `loadLibrary*` ones were pure no-ops).
    //
    // They were not redundant, they were HARMFUL, and the comment that
    // justified them was measurably wrong. It claimed real-JDK mode "calls
    // `register_essential_natives` ... but NOT `register_runtime_natives`";
    // `register_essential_natives_with_shims` calls
    // `lang_system::register_runtime_natives` unconditionally
    // (`native-builtins/src/lib.rs`), and vm_init runs it BEFORE
    // `register_vm_management_impl` in both the real-JDK and synthetic
    // branches. Registration is last-write-wins, so these no-ops silently
    // replaced `lang_system`'s REAL implementations — the ones that map a bare
    // library name through `platform_lib_name` and call
    // `ctx.load_native_library`. Net effect: no JNI library could ever be
    // loaded by `System.loadLibrary` anywhere in the VM.
    //
    // Deleting them is safe in both run modes: `lang_system` registers all four
    // triples in both, its bodies already swallow load failures
    // (`let _ = ctx.load_native_library(..)`), so nothing they can do throws —
    // which is the only property `ManagementFactory.<clinit>` needed — and the
    // narrow `loadNativeLib` short-circuit above means that chain never reaches
    // them anyway. `vm_exec.rs`'s force-native-override allowlist for these
    // triples is unaffected: it forces WHATEVER native is registered, and that
    // is now the real loader instead of a no-op.
    r.register(
        "java/lang/management/ManagementFactory",
        "loadNativeLib",
        "()V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP: returning normally without installing anything IS the correct
    // report. `LinuxNativeAccess.tryInstallExecSandbox()` is defined to *try*,
    // and to leave `execSandboxState` at its default when it cannot — CratonVM
    // installs no process-wide seccomp filter, so the untouched default
    // (`NONE`) is what `getExecSandboxState()` should and does answer. The
    // method's own contract is why this is not a swallowed failure.
    r.register(
        "org/elasticsearch/nativeaccess/LinuxNativeAccess",
        "tryInstallExecSandbox",
        "()V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );

    // Wave 1 / Task A: short-circuit `ManagementFactory.getPlatformMXBeans
    // (Class<? extends PlatformManagedObject>)List` — the bytecode body
    // routes through `PlatformMBeanFinder.findFirst` + `PlatformComponent`
    // SPI loading, neither of which is wired up in our VM. The convenience
    // wrappers `getMemoryPoolMXBeans()` / `getMemoryManagerMXBeans()` /
    // `getGarbageCollectorMXBeans()` all flow through this method, so a
    // single override populates all three lists at once.
    // Wave 1 / Task A: short-circuit `ManagementFactory.getPlatformMXBean
    // (Class<? extends PlatformManagedObject>)T`. The real bytecode routes
    // through `PlatformMBeanFinder`; in real-JDK mode that nested helper can be
    // left unresolved even though callers only need conservative MXBean values.
    r.register(
        "java/lang/management/ManagementFactory",
        "getPlatformMXBean",
        "(Ljava/lang/Class;)Ljava/lang/management/PlatformManagedObject;",
        |ctx, args| {
            let cls_arg = obj_arg(args, 0).ok();
            let cls_name: String = cls_arg
                .and_then(|c| ctx.class_id_from_mirror(c))
                .and_then(|id| ctx.class_name_of_id(id))
                .unwrap_or_default();
            let bean = match cls_name.as_str() {
                "java/lang/management/RuntimeMXBean" => Some(alloc_runtime_mxbean(ctx)),
                // gen r5w3/obs7: the VM's one MemoryMXBean.
                "java/lang/management/MemoryMXBean" => Some(platform_memory_mxbean(ctx)),
                "java/lang/management/ThreadMXBean" => Some(alloc_thread_mxbean(ctx)),
                "java/lang/management/ClassLoadingMXBean" => Some(alloc_class_loading_mxbean(ctx)),
                "java/lang/management/OperatingSystemMXBean"
                | "com/sun/management/OperatingSystemMXBean" => Some(alloc_os_mxbean(ctx)),
                "java/lang/management/CompilationMXBean" => Some(alloc_compilation_mxbean(ctx)),
                "java/lang/management/PlatformLoggingMXBean" => Some(alloc_logging_mxbean(ctx)),
                "jdk/management/VirtualThreadSchedulerMXBean" => {
                    Some(alloc_virtual_thread_scheduler_mxbean(ctx))
                }
                // Optional HotSpot-only diagnostics. Returning null mirrors a
                // JVM without that platform bean and lets Elasticsearch keep
                // its documented fallback defaults for these VM options.
                // zt t26/x21: HotSpot 25 answers its one `HotSpotDiagnostic`
                // here; so does this native now
                // (`CRATONVM_JMX_HOTSPOT_DIAGNOSTIC_BEAN`, `=0`: null again).
                "com/sun/management/HotSpotDiagnosticMXBean" => {
                    jdk_hotspot_diagnostic_mxbean(ctx).map(Ok)
                }
                _ => None,
            };
            Ok(Some(Value::Object(bean.transpose()?)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

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
                // Three pools, in HotSpot's own order — see `BUFFER_POOL_NAMES`.
                // Until 2026-08-22 this fell to the empty-list arm below, so
                // `getPlatformMXBeans(BufferPoolMXBean.class)` answered a list
                // with no "direct" pool in it and direct-buffer allocation was
                // not observable from Java at all.
                "java/lang/management/BufferPoolMXBean" => {
                    let pools = cratonvm_native_builtins::shared_secrets_bridge::alloc_all_buffer_pools(ctx)?;
                    // Empty means `--real-jdk` refused the `cratonvm/internal/
                    // BufferPool` carrier, which is the policy working as
                    // designed: strict mode forbids compatibility stand-ins.
                    // Answering an EMPTY LIST is not, and it is the exact harm
                    // `bug-the-bufferpool-refusal-takes-out-the-whole-platform-
                    // mbean-server-20260822-FIXED-20260901.md` argued a loud
                    // refusal was preferable to.
                    //
                    // MEASURED on 2026-09-01, `probes/JmxBlast.java`, one
                    // binary, the two modes:
                    //
                    //   compatible   pools = 3, direct count 0 -> 1 on a
                    //                1 MiB allocateDirect
                    //   --real-jdk   pools = 0
                    //
                    // Strict mode HAS a real class library, so there is a real
                    // answer to hand: `ManagementFactoryHelper` builds its
                    // three beans over `Bits.BUFFER_POOL` and the two
                    // `FileChannelImpl` mapped pools, which is what HotSpot
                    // answers with. It is only reachable BECAUSE
                    // `JavaNioAccess.getDirectBufferPool` was retired
                    // (`ba798eca7`) — the shim that used to sit in front of it
                    // is what took out the whole platform MBean server. This
                    // is the delegation that fix made possible.
                    if pools.is_empty() {
                        // The JDK could not answer either (a synthetic image,
                        // where there is no `ManagementFactoryHelper`) — fall
                        // through to the empty list, which is what this arm did
                        // before and is still better than a throwable out of a
                        // `<clinit>`.
                        cratonvm_native_builtins::shared_secrets_bridge::jdk_buffer_pools_in_hotspot_order(ctx)
                            .unwrap_or(pools)
                    } else {
                        pools
                    }
                }
                // gen r4w2/obs: the collector's own beans when it describes
                // them (see "Per-generation beans"); the historical pair and
                // single `G1 Young Generation` bean otherwise.
                //
                // gen r4w3/obs2: and on that path, the SAME beans on every call —
                // the JDK's own `MemoryImpl` caches (`MemoryImpl.getMemoryPools()`
                // / `getMemoryManagers()`), as HotSpot hands out one bean per
                // pool and manager. A fresh bean per call made
                // `addNotificationListener` register on an object nothing else
                // could reach. Fresh beans only if the cache cannot be read.
                "java/lang/management/MemoryPoolMXBean" => {
                    let names: Vec<&'static str> =
                        ctx.gc_memory_pools().iter().map(|p| p.name).collect();
                    if names.is_empty() {
                        // gc-common w16-f: the first bean is rooted across the
                        // second's allocation (the `vec!` held it bare).
                        let eden = alloc_memory_pool_impl(ctx, "Eden Space", true)?;
                        let mut scope = NativeHandleScope::new(ctx);
                        let eden_h = scope.root(eden);
                        let old = alloc_memory_pool_impl(&mut *scope, "Old Gen", true)?;
                        vec![scope.get(&eden_h), old]
                    } else {
                        let arr = match canonical_pool_beans(ctx) {
                            Some(arr) => arr,
                            // gci i15/nh: the same list `getMemoryPools0` builds.
                            None => {
                                let pool_names = pool_names_with_non_heap(&*ctx, &names);
                                let arr = alloc_pool_bean_array(ctx, &pool_names)?;
                                // zt t1/ob: handed out as `getMemoryPools0`
                                // hands them out, so ZGC's edges sample too.
                                ctx.note_non_heap_pools_published();
                                arr
                            }
                        };
                        bean_array_to_vec(ctx, arr)
                    }
                }
                // gen r4w3/obs2: `com.sun.management.GarbageCollectorMXBean`
                // joined this arm (it was the empty `_` fallback, which it
                // still is when the collector describes no beans).
                "java/lang/management/MemoryManagerMXBean"
                | "java/lang/management/GarbageCollectorMXBean"
                | "com/sun/management/GarbageCollectorMXBean" => {
                    let ext_only = cls_name == "com/sun/management/GarbageCollectorMXBean";
                    let names: Vec<&'static str> =
                        ctx.gc_collectors().iter().map(|c| c.name).collect();
                    if names.is_empty() {
                        if ext_only {
                            Vec::new()
                        } else {
                            vec![alloc_garbage_collector_impl(ctx, "G1 Young Generation")?]
                        }
                    } else {
                        let arr = match canonical_collector_beans(ctx) {
                            Some(arr) => arr,
                            None => alloc_manager_bean_array(ctx, &names)?,
                        };
                        let mut beans = bean_array_to_vec(ctx, arr);
                        if ext_only {
                            beans.retain(|b| is_gc_ext_bean(&*ctx, *b));
                        } else if cls_name == "java/lang/management/GarbageCollectorMXBean" {
                            // gci i15/nh: the manager list now starts with the
                            // non-heap managers, which are no collectors.
                            retain_collector_beans(&*ctx, &mut beans, &names);
                        }
                        beans
                    }
                }
                // Other PlatformManagedObject classes (RuntimeMXBean, etc.)
                // hit a separate `getPlatformMXBean` (singleton) path; the
                // list-form fallback is an empty list, matching the behaviour
                // of a JVM that genuinely has no extra components registered
                // for that interface.
                _ => Vec::new(),
            };
            // gen r4w2/obs: the beans are rooted across the two allocations
            // below (the backing array and the list). They were held as bare
            // references across both, which a moving collection in either
            // would have left pointing at vacated memory; the per-generation
            // arms above made the list three beans long.
            let mut scope = NativeHandleScope::new(ctx);
            let handles: Vec<_> = beans.iter().map(|b| scope.root(*b)).collect();
            let backing = scope.new_ref_array(ClassId::new(0), handles.len());
            let backing_h = scope.root(backing);
            for (i, h) in handles.iter().enumerate() {
                let b = scope.get(h);
                let backing = scope.get(&backing_h);
                scope.set_array_element(backing, i, Value::Object(Some(b)));
            }
            let list: ObjectRef =
                try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
            let backing = scope.get(&backing_h);
            scope.set_field_by_name(list, "elementData", Value::Object(Some(backing)));
            scope.set_field_by_name(list, "size", Value::Int(handles.len() as i32));
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    register_jmx_connector_factory(r);
    register_diagnostic_command_impl(r);
    }

// ---------------------------------------------------------------------------
// com.sun.management.internal.DiagnosticCommandImpl (gci i28/dg)
// ---------------------------------------------------------------------------

/// `CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN` (default ON since 2026-10-17, `=0`
/// kills; gci i28/dg, `runtime_flag_default_on`;
/// `gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 11b): `VMManagementImpl.remoteDiagnosticCommandsSupport` is HotSpot's
/// `1`, so `ManagementFactory.getPlatformMBeanServer()` registers the
/// `com.sun.management:type=DiagnosticCommand` bean over
/// [`register_diagnostic_command_impl`]'s natives. Off: `0`, no bean (as
/// before). Read once per `VMManagementImpl.<clinit>` and per support query;
/// no static.
pub(crate) fn jmx_diagnostic_command_mbean_enabled() -> bool {
    true
}

pub(in crate::jmx) const DCMD_IMPL: &str = "com/sun/management/internal/DiagnosticCommandImpl";
pub(in crate::jmx) const DCMD_INFO: &str = "com/sun/management/internal/DiagnosticCommandInfo";
pub(in crate::jmx) const DCMD_INFO_CTOR: &str = "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;\
Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZLjava/util/List;)V";
/// gci i32/jh: JDK 25's only `DiagnosticCommandInfo` constructor (no
/// permission triple; `gci-i32jh-dcmd-mbean-builds-diagnosticcommandinfo-with-the-jdk21-constructor-FIXED-20261016`).
pub(in crate::jmx) const DCMD_INFO_CTOR_25: &str =
    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZLjava/util/List;)V";
pub(in crate::jmx) const DCMD_ARG_INFO: &str = "com/sun/management/internal/DiagnosticCommandArgumentInfo";
pub(in crate::jmx) const DCMD_ARG_INFO_CTOR: &str =
    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ZZZI)V";

/// gci i28/dg: the four `native` methods of
/// `com.sun.management.internal.DiagnosticCommandImpl` (HotSpot's
/// `libmanagement_ext` `DiagnosticCommandImpl.c` over `jmm_interface`), each
/// a `Bridge` stated at its site -- a JDK `native` method has no bytecode to
/// lose to. Registered unconditionally: they are reachable only through the
/// bean `ManagementFactoryHelper` builds when
/// [`jmx_diagnostic_command_mbean_enabled`] seeds the support field, so with
/// the switch off nothing calls them.
///
/// - `getDiagnosticCommands()` -- [`NativeContext::dcmd_names`].
/// - `getDiagnosticCommandInfo(String[])` -- one `DiagnosticCommandInfo` per
///   name, its arguments a `List` built by `Arrays.asList` over a
///   `DiagnosticCommandArgumentInfo[]` (as the C side does); a `null` array is
///   `NullPointerException("Invalid String Array")`, a `null` element
///   `NullPointerException("Command name cannot be null.")`, an unknown name
///   `IllegalArgumentException("Unknown diagnostic command")` (`jmm_GetDiagnosticCommandInfo`).
/// - `executeDiagnosticCommand(String)` -- [`NativeContext::dcmd_execute`]'s
///   output, or its `IllegalArgumentException`.
/// - `setNotificationEnabled(boolean)` -- accepted; this VM's command set never
///   changes after start, so there is never a notification to send.
///
/// Idempotent: the shipping boot path reaches [`register_vm_management_impl`]
/// twice (directly and through [`register_jmx_natives`]), and a second
/// registration of these triples would only add four shadowed losers to
/// `native-builtins/tests/duplicate_registration_gate.rs`'s frozen count.
pub fn register_diagnostic_command_impl(r: &mut NativeMethodRegistry) {
    if r
        .kind_of(DCMD_IMPL, "getDiagnosticCommands", "()[Ljava/lang/String;")
        .is_some()
    {
        return;
    }
    r.register(
        DCMD_IMPL,
        "getDiagnosticCommands",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // `NewStringUTF` strings, as HotSpot's: fresh, not interned.
            let names = ctx.dcmd_names();
            let cls = ctx.ensure_class_initialized("java/lang/String")?;
            let mut scope = NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(cls, names.len());
            let arr_h = scope.root(arr);
            for (i, name) in names.iter().enumerate() {
                let s = scope.create_string_uninterned(name);
                let arr = scope.get(&arr_h);
                scope.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        DCMD_IMPL,
        "getDiagnosticCommandInfo",
        "([Ljava/lang/String;)[Lcom/sun/management/internal/DiagnosticCommandInfo;",
        |ctx, args| {
            let names_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Invalid String Array".to_string()),
                    }
                    .into())
                }
            };
            let count = ctx.array_length(names_arr);
            let mut infos = Vec::with_capacity(count);
            for i in 0..count {
                let Value::Object(Some(element)) = ctx.get_array_element(names_arr, i) else {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Command name cannot be null.".to_string()),
                    }
                    .into());
                };
                let name = ctx.read_string(element).unwrap_or_default();
                match ctx.dcmd_info(&name) {
                    Some(info) => infos.push(info),
                    None => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "Unknown diagnostic command".to_string(),
                        }
                        .into())
                    }
                }
            }
            let out = build_dcmd_info_array(ctx, &infos)?;
            Ok(Some(Value::Object(Some(out))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        DCMD_IMPL,
        "executeDiagnosticCommand",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let line = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Invalid command".to_string()),
                    }
                    .into())
                }
            };
            match ctx.dcmd_execute(&line) {
                Ok(output) => {
                    let s = ctx.create_string_uninterned(&output);
                    Ok(Some(Value::Object(Some(s))))
                }
                Err(message) => Err(RuntimeError::IllegalArgumentException { message }.into()),
            }
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        DCMD_IMPL,
        "setNotificationEnabled",
        "(Z)V",
        |_ctx, _args| Ok(None),
        cratonvm_native_api::NativeKind::Bridge,
    );
}

/// gci i28/dg: a `String` for an optional text, `null` for `None`.
pub(in crate::jmx) fn dcmd_opt_string(ctx: &mut dyn NativeContext, s: &Option<String>) -> Value {
    match s {
        Some(s) => Value::Object(Some(ctx.create_string_uninterned(s))),
        None => Value::Object(None),
    }
}

/// gci i28/dg: `DiagnosticCommandInfo[]` for `infos`, every object rooted in
/// one handle scope across the allocations that follow it (the array, each
/// argument array, each `List`, each string and constructor may collect).
pub(in crate::jmx) fn build_dcmd_info_array(
    ctx: &mut dyn NativeContext,
    infos: &[cratonvm_native_api::registry::DcmdInfo],
) -> Result<ObjectRef, MethodCallFailed> {
    let info_cls = ctx.ensure_class_initialized(DCMD_INFO)?;
    let arg_cls = ctx.ensure_class_initialized(DCMD_ARG_INFO)?;
    // gci i32/jh: the library's own constructor shape (JDK 25: five
    // arguments; JDK 17/21: eight, with the permission triple).
    let ctor_25 = ctx.method_exists(DCMD_INFO, "<init>", DCMD_INFO_CTOR_25);
    let mut scope = NativeHandleScope::new(ctx);
    let out = scope.new_ref_array(info_cls, infos.len());
    let out_h = scope.root(out);
    for (i, info) in infos.iter().enumerate() {
        // The arguments: `DiagnosticCommandArgumentInfo[]` -> `Arrays.asList`.
        let args_arr = scope.new_ref_array(arg_cls, info.arguments.len());
        let args_h = scope.root(args_arr);
        for (j, a) in info.arguments.iter().enumerate() {
            let texts = [
                Some(a.name.clone()),
                Some(a.description.clone()),
                Some(a.arg_type.clone()),
                a.default_value.clone(),
            ];
            let mut held: Vec<Option<NativeHandle>> = Vec::with_capacity(texts.len());
            for text in &texts {
                held.push(match dcmd_opt_string(&mut *scope, text) {
                    Value::Object(Some(o)) => Some(scope.root(o)),
                    _ => None,
                });
            }
            let mut ctor_args: Vec<Value> = held
                .iter()
                .map(|h| match h {
                    Some(h) => Value::Object(Some(scope.get(h))),
                    None => Value::Object(None),
                })
                .collect();
            ctor_args.push(Value::Int(i32::from(a.mandatory)));
            ctor_args.push(Value::Int(i32::from(a.option)));
            ctor_args.push(Value::Int(i32::from(a.multiple)));
            ctor_args.push(Value::Int(a.position));
            let arg_obj = match scope.new_object_initialized(
                DCMD_ARG_INFO,
                DCMD_ARG_INFO_CTOR,
                &ctor_args,
            )? {
                Some(Value::Object(Some(o))) => o,
                _ => continue,
            };
            let args_arr = scope.get(&args_h);
            scope.set_array_element(args_arr, j, Value::Object(Some(arg_obj)));
        }
        let args_arr = scope.get(&args_h);
        let list = match scope.invoke_static(
            "java/util/Arrays",
            "asList",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(args_arr))],
        )? {
            Some(Value::Object(Some(l))) => l,
            _ => continue,
        };
        let list_h = scope.root(list);
        let mut texts = vec![
            Some(info.name.clone()),
            Some(info.description.clone()),
            Some(info.impact.clone()),
        ];
        if !ctor_25 {
            texts.push(info.permission_class.clone());
            texts.push(info.permission_name.clone());
            texts.push(info.permission_action.clone());
        }
        let mut held: Vec<Option<NativeHandle>> = Vec::with_capacity(texts.len());
        for text in &texts {
            held.push(match dcmd_opt_string(&mut *scope, text) {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            });
        }
        let mut ctor_args: Vec<Value> = held
            .iter()
            .map(|h| match h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => Value::Object(None),
            })
            .collect();
        ctor_args.push(Value::Int(i32::from(info.enabled)));
        ctor_args.push(Value::Object(Some(scope.get(&list_h))));
        let ctor = if ctor_25 { DCMD_INFO_CTOR_25 } else { DCMD_INFO_CTOR };
        let info_obj = match scope.new_object_initialized(DCMD_INFO, ctor, &ctor_args)? {
            Some(Value::Object(Some(o))) => o,
            _ => continue,
        };
        let out = scope.get(&out_h);
        scope.set_array_element(out, i, Value::Object(Some(info_obj)));
    }
    Ok(scope.get(&out_h))
}

// ---------------------------------------------------------------------------
// javax.management.remote.JMXConnectorFactory
//
// Cassandra's `nodetool version` (and any JMX client) calls
// `JMXConnectorFactory.connect(serviceURL)` which delegates to
// `newJMXConnector(serviceURL, env)`. That static method uses
// `ServiceLoader.load(JMXConnectorProvider.class)` to find a per-protocol
// provider. For the `rmi` protocol the provider class
// `com.sun.jmx.remote.protocol.rmi.ClientProvider` is declared in the
// `java.management.rmi` module via `module-info: provides ... with ...`
// — **not** via a `META-INF/services/...` descriptor. CratonVM's
// `ServiceLoader` (native-builtins/src/service_loader.rs) only reads the
// classpath `META-INF/services/` form and therefore returns zero
// providers for `JMXConnectorProvider`. `JMXConnectorFactory` then
// throws `MalformedURLException("Unsupported protocol: rmi")`, which
// nodetool surfaces verbatim:
//
//     nodetool: Failed to connect to '127.0.0.1:7199' \
//         - MalformedURLException: 'Unsupported protocol: rmi'.
//
// Implementing the full RMI stack (RMIConnector, JRMP, stub/skeleton,
// remote method dispatch) is out of scope; we don't have an RMI runtime.
//
// BUT: not every JMX protocol needs the RMI stack. Spring Framework's jmx.*
// test suite (`ConnectorServerFactoryBeanTests`, `MBeanServerConnectionFactoryBeanTests`,
// `RemoteMBeanClientInterceptorTests`, ...) connects over `jmxmp`
// (`service:jmx:jmxmp://...`), backed by `org.glassfish.external:
// opendmk_jmxremote_optional_jar` — a classic pre-JPMS jar whose
// `com.sun.jmx.remote.protocol.jmxmp.{ClientProvider,ServerProvider}` classes
// ARE declared via plain `META-INF/services/javax.management.remote.
// JMXConnectorProvider` (and `...JMXConnectorServerProvider`) descriptors.
// That is exactly the classpath-scanning form our own `ServiceLoader`
// supports — real bytecode for `jmxmp` would work end-to-end if it ran.
//
// So: instead of unconditionally raising the canned "not implemented"
// error, look up real `JMXConnectorProvider` instances ourselves (via the
// same `ServiceLoader.load` + `iterator()` natives real JDK bytecode would
// use) and delegate to whichever provider accepts the URL's protocol —
// this covers `jmxmp` (and any other classpath-declared provider) with
// the REAL provider implementation, no synthetic stand-in. Only fall back
// to the "not implemented" IOException when no provider on the classpath
// claims the protocol (still exactly right for `rmi`, whose provider is a
// JPMS module our ServiceLoader can't see) — preserving nodetool's
// existing clean-exit UX:
//
//     nodetool: Failed to connect to '127.0.0.1:7199' \
//         - IOException: 'JMX over RMI is not implemented in CratonVM …'.
// ---------------------------------------------------------------------------
pub(in crate::jmx) fn register_jmx_connector_factory(r: &mut NativeMethodRegistry) {
    r.register(
        "javax/management/remote/JMXConnectorFactory",
        "newJMXConnector",
        "(Ljavax/management/remote/JMXServiceURL;Ljava/util/Map;)Ljavax/management/remote/JMXConnector;",
        |ctx, args| {
            let url_val = args.first().copied().unwrap_or(Value::Object(None));
            let env_val = args.get(1).copied().unwrap_or(Value::Object(None));

            if let Some(connector_or_err) = try_delegate_to_real_provider(
                ctx,
                "javax/management/remote/JMXConnectorProvider",
                "newJMXConnector",
                "(Ljavax/management/remote/JMXServiceURL;Ljava/util/Map;)Ljavax/management/remote/JMXConnector;",
                &[url_val, env_val],
            ) {
                return connector_or_err;
            }

            // No classpath-declared provider claimed this protocol.
            // Reconstruct the service URL for the error message. JMXServiceURL
            // is a real-JDK class; its `toString` returns
            // `service:jmx:<protocol>://<host>:<port><path>`. If the call fails
            // (e.g. argument is null) we still raise a meaningful IOException
            // so the caller's catch surfaces the right error class.
            let url_str = match url_val {
                Value::Object(Some(u)) => ctx
                    .invoke_virtual(u, "toString", "()Ljava/lang/String;", &[])
                    .ok()
                    .and_then(|v| v)
                    .and_then(|v| match v {
                        Value::Object(Some(s)) => ctx.read_string(s),
                        _ => None,
                    })
                    .unwrap_or_else(|| String::from("<unknown JMX URL>")),
                _ => String::from("<null JMX URL>"),
            };
            Err(jmx_ioex(format!(
                "JMX over RMI is not implemented in CratonVM \
                 (cannot establish RMI connection to {url_str})"
            )))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// Look up real `provider_iface` instances via `ServiceLoader.load` +
/// `iterator()` (both already-real natives in `service_loader.rs`) and try
/// each one's `factory_method(url, env)` in turn, mirroring what real-JDK
/// `JMXConnectorFactory`/`JMXConnectorServerFactory` bytecode does via
/// `getConnectorAsService`/`ProviderFinder`.
///
/// Returns `None` when no provider was found at all (or every provider
/// rejected the URL with `MalformedURLException`, meaning "protocol not
/// recognized by any provider on the classpath") — callers should fall
/// back to their own "unsupported" handling in that case. Returns
/// `Some(Ok(v))` on the first provider that successfully produced a
/// connector/connector-server, or `Some(Err(..))` if a provider raised
/// some OTHER exception (surfaced as-is, since that is real bytecode's own
/// diagnosis of a genuine failure, more accurate than a canned message).
///
/// Simplification vs. real JDK's `ProviderFinder`: a non-`MalformedURLException`
/// failure from one provider is remembered but does not stop the search
/// (real JDK short-circuits immediately on `JMXProviderException`). With
/// only one provider realistically ever on a classpath (there's no second
/// JMX transport jar to conflict with), this is behavior-identical in
/// practice and avoids replicating JDK's internal stream/predicate plumbing.
pub(in crate::jmx) fn try_delegate_to_real_provider(
    ctx: &mut dyn NativeContext,
    provider_iface: &str,
    factory_method: &str,
    factory_descriptor: &str,
    factory_args: &[Value],
) -> Option<MethodCallResult> {
    let iface_id = ctx.ensure_class_initialized(provider_iface).ok()?;
    let mirror = ctx.get_class_mirror(iface_id);
    // gc-common w19-e: everything held across the Java calls below is rooted in
    // one scope -- the factory arguments (the caller's `JMXServiceURL` and
    // environment map, passed to EVERY provider after `ServiceLoader.load`,
    // `hasNext` and `next` had run), the iterator, and the first exception,
    // which was held raw across every later provider's calls. The iterator pin
    // also used to be left behind.
    let mut scope = NativeHandleScope::new(ctx);
    let arg_hs: Vec<Option<NativeHandle>> = factory_args
        .iter()
        .map(|v| match *v {
            Value::Object(Some(o)) => Some(scope.root(o)),
            _ => None,
        })
        .collect();
    let loader_obj: ObjectRef = match scope.invoke(
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(mirror))],
    ) {
        Ok(Some(Value::Object(Some(sl)))) => sl,
        _ => return None,
    };
    let iter_obj: ObjectRef =
        match scope.invoke_virtual(loader_obj, "iterator", "()Ljava/util/Iterator;", &[]) {
            Ok(Some(Value::Object(Some(it)))) => it,
            _ => return None,
        };
    let iter_h = scope.root(iter_obj);

    let mut first_exception: Option<NativeHandle> = None;
    loop {
        let iter_now = scope.get(&iter_h);
        match scope.invoke_virtual(iter_now, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        let iter_now = scope.get(&iter_h);
        let provider_obj: ObjectRef =
            match scope.invoke_virtual(iter_now, "next", "()Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(p)))) => p,
                _ => break,
            };
        let args_now: Vec<Value> = factory_args
            .iter()
            .zip(&arg_hs)
            .map(|(v, h)| match h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => *v,
            })
            .collect();
        match scope.invoke_virtual(provider_obj, factory_method, factory_descriptor, &args_now) {
            Ok(Some(v)) => return Some(Ok(Some(v))),
            Ok(None) => {}
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                let is_malformed = scope
                    .class_name_arc_of_id(scope.class_id_of_object(exc))
                    .as_deref()
                    == Some("java/net/MalformedURLException");
                if !is_malformed && first_exception.is_none() {
                    first_exception = Some(scope.root(exc));
                }
            }
            Err(internal) => return Some(Err(internal)),
        }
    }

    first_exception.map(|h| Err(MethodCallFailed::ExceptionThrown(scope.get(&h))))
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

    // ThreadImpl.getThreadInfo(long, int) allocates an output array and asks
    // this native to populate it. Fill a minimal but real ThreadInfo so callers
    // that only need a stack-trace object do not see a fabricated null thread.
    r.register(
        cls,
        "getThreadInfo1",
        "([JI[Ljava/lang/management/ThreadInfo;)V",
        |ctx, args| {
            let ids = obj_arg(args, 0)?;
            let out = obj_arg(args, 2)?;
            // `maxDepth`: -1 for the whole stack (`ThreadImpl` passes it for
            // `Integer.MAX_VALUE`), else at most that many innermost frames.
            let max_depth = match args.get(1) {
                Some(Value::Int(d)) => *d,
                _ => -1,
            };
            let ids_pin = ctx.pin_native_root(ids);
            let out_pin = ctx.pin_native_root(out);
            let len = ctx.array_length(ids).min(ctx.array_length(out));
            // JIT round 25 W10-7 review: a dump of several `ThreadInfo`s with
            // frames reads them all in its own pauses
            // (`begin_thread_info_dump`), not in pauses of its own per target.
            let dumping = len > 1 && max_depth != 0;
            if dumping {
                let targets: Vec<ObjectRef> = read_long_array(ctx, ids)
                    .into_iter()
                    .take(len)
                    .filter(|&id| id > 0)
                    .filter_map(|id| platform_thread_object_for_java_tid(ctx, id))
                    .collect();
                ctx.begin_thread_info_dump(&targets);
            }
            let filled = thread_info1_fill(ctx, (ids_pin, ids), (out_pin, out), len, max_depth);
            if dumping {
                ctx.end_thread_dump();
            }
            ctx.unpin_native_roots(ids_pin);
            filled
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // Interpreter round i4 wave 84, lane L6: NOT a no-op. `ThreadImpl
    // .setThreadCpuTimeEnabled(false)` reaches this STATIC native (the boolean
    // is `args[0]`), and HotSpot's `jmm_SetBoolAttribute(THREAD_CPU_TIME)`
    // then makes `VMManagementImpl.isThreadCpuTimeEnabled()` false and every
    // CPU-time getter `-1`. The no-op left both answering as if enabled
    // (`L6W84ThreadMxBeanShapes`). The same per-VM flag the getters read
    // (`cpu_time_for_requested_tid`) and the synthetic bean's setter writes.
    r.register(
        cls,
        "setThreadCpuTimeEnabled0",
        "(Z)V",
        |ctx, args| {
            let enable = !matches!(args.first(), Some(Value::Int(0)));
            jmm_switch_set(ctx, |s| s.thread_cpu_time = enable);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // Thread allocated-memory accounting, the real-JDK twin of
    // `register_thread_mxbean_extensions`. Real `ThreadImpl.getThreadAllocatedBytes`
    // is `verifyThreadAllocatedMemory` (throws `UnsupportedOperationException`
    // unless `VMManagementImpl.isThreadAllocatedMemorySupported()`) and then one
    // of the natives below, so reporting "unsupported" made netty's
    // `AbstractByteBufAllocatorTest.shouldReuseChunks` FAIL where HotSpot passes.
    // The same rule as the interface twin: only the CALLING thread is measurable
    // (its TLAB cursor may not be read while another thread owns it), every other
    // id answers the JMM's `-1`, and `0` means "the current thread".
    r.register(
        cls,
        "getThreadAllocatedMemory0",
        "(J)J",
        |ctx, args| {
            let requested = match args.first() {
                Some(Value::Long(id)) if *id != 0 => Some(*id),
                _ => None,
            };
            let bytes = if current_thread_tid_matches(ctx, requested) {
                ctx.current_thread_allocated_bytes()
            } else {
                None
            };
            Ok(Some(Value::Long(
                bytes.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)),
            )))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getThreadAllocatedMemory1",
        "([J[J)V",
        |ctx, args| {
            let ids = obj_arg(args, 0)?;
            let out = obj_arg(args, 1)?;
            let requested = read_long_array(ctx, ids);
            let ids_pin = ctx.pin_native_root(ids);
            let out_pin = ctx.pin_native_root(out);
            for (i, id) in requested.iter().enumerate() {
                let bytes = if *id != 0 && current_thread_tid_matches(ctx, Some(*id)) {
                    ctx.current_thread_allocated_bytes()
                } else {
                    None
                };
                let out = ctx.read_native_pin(out_pin, out);
                ctx.set_array_element(
                    out,
                    i,
                    Value::Long(bytes.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX))),
                );
            }
            ctx.unpin_native_roots(out_pin);
            ctx.unpin_native_roots(ids_pin);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getTotalThreadAllocatedMemory",
        "()J",
        |ctx, _args| {
            let total = ctx
                .total_allocated_bytes()
                .unwrap_or_else(|| ctx.heap_allocated_bytes() as u64);
            Ok(Some(Value::Long(i64::try_from(total).unwrap_or(i64::MAX))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "setThreadAllocatedMemoryEnabled0",
        "(Z)V",
        |ctx, args| {
            let enable = args.first().and_then(|v| v.as_int()).unwrap_or(0) != 0;
            jmm_switch_set(ctx, |s| s.thread_allocated_memory = enable);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "setThreadContentionMonitoringEnabled0",
        "(Z)V",
        native_set_thread_contention_monitoring_enabled,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "resetContentionTimes0",
        "(J)V",
        |ctx, _args| {
            ctx.reset_thread_jmx_contention_stats();
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // REAL — arbitrary-thread CPU/user time on the real-JDK surface, the twin of
    // `ThreadMXBean.getThreadCpuTime(J)` in `register_thread_mxbean`. Now that
    // `VMManagementImpl.isOtherThreadCpuTimeSupported()` answers true,
    // `ThreadImpl.verifyThreadCpuTime` stops short-circuiting and these DO get
    // called, so leaving them no-ops would make the real-JDK bean promise a
    // measurement and then hand back the pre-filled -1s.
    //
    // DESCRIPTORS (verified against JDK 25 bytecode, not assumed): the JDK
    // declares `getThreadTotalCpuTime0(long)` / `getThreadUserCpuTime0(long)`
    // returning `long`, and the `…1(long[], long[])` array forms — the same
    // `0`=scalar / `1`=array split as `getThreadInfo1` and
    // `getThreadAllocatedMemory1` above. The previous registrations bound the
    // `0` names to `([J[J)V` — a descriptor that appears nowhere in `ThreadImpl`
    // — so they could never dispatch and are dropped here rather than kept as
    // dead entries; and the `1` forms were absent entirely, which would have
    // been an UnsatisfiedLinkError the moment the support flag let a bulk query
    // through. All four correct shapes are registered now.
    //
    // Id 0 means "the current thread" in the JDK's own calling convention
    // (`getCurrentThreadCpuTime()` compiles to `getThreadTotalCpuTime0(0L)`),
    // and `cpu_time_for_requested_tid` treats a `None` id the same way.
    let total_cpu_time_scalar: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, args| {
            let requested = requested_thread_id(args).filter(|id| *id != 0);
            Ok(Some(Value::Long(
                cpu_time_for_requested_tid(ctx, requested).map_or(-1, |(cpu, _user)| cpu),
            )))
        };
    let user_cpu_time_scalar: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |ctx, args| {
            let requested = requested_thread_id(args).filter(|id| *id != 0);
            Ok(Some(Value::Long(
                cpu_time_for_requested_tid(ctx, requested).map_or(-1, |(_cpu, user)| user),
            )))
        };
    r.register(
        cls,
        "getThreadTotalCpuTime0",
        "(J)J",
        total_cpu_time_scalar,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getThreadUserCpuTime0",
        "(J)J",
        user_cpu_time_scalar,
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getThreadTotalCpuTime1",
        "([J[J)V",
        |ctx, args| fill_thread_cpu_times(ctx, args, false),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getThreadUserCpuTime1",
        "([J[J)V",
        |ctx, args| fill_thread_cpu_times(ctx, args, true),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL — `dumpThreads0(long[] ids, boolean lockedMonitors, boolean
    // lockedSynchronizers, int maxDepth)`, STATIC, so ids is `args[0]` and a
    // null ids means "every live thread". This was UNREGISTERED, which was
    // survivable only while `VMManagementImpl.isSynchronizerUsageSupported()`
    // answered false: `ThreadImpl.verifyDumpThreads` threw
    // UnsupportedOperationException before ever reaching the native. Now that
    // the flag is true, `getThreadInfo(ids, false, true)` and
    // `dumpAllThreads(false, true)` both land here, so registering it is what
    // keeps the flip from turning into an UnsatisfiedLinkError.
    //
    // Interpreter round i2 wave 60, lane L6: the two booleans and `maxDepth`
    // are applied as HotSpot applies them (`limit_thread_info_snapshot`;
    // `getThreadInfo(ids, false, false)` lists no monitor there). They were
    // ignored, on the reading that more than asked for is spec-legal; both
    // modes are held to HotSpot's answer.
    r.register(
        cls,
        "dumpThreads0",
        "([JZZI)[Ljava/lang/management/ThreadInfo;",
        |ctx, args| {
            let ids = match args.first() {
                Some(Value::Object(Some(arr))) => read_long_array(ctx, *arr),
                _ => live_thread_ids(ctx),
            };
            let flag = |i: usize| !matches!(args.get(i), Some(Value::Int(0)));
            let max_depth = match args.get(3) {
                Some(Value::Int(d)) => *d,
                _ => -1,
            };
            thread_info_array_for_ids_limited(ctx, &ids, (flag(1), flag(2), max_depth))        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // REAL: `resetPeakThreadCount0` left the batch above — it is the native
    // behind `ThreadMXBean.resetPeakThreadCount()`, and there is a real
    // high-water mark to reset now (see `peak_thread_count`). Shares the one
    // process-wide counter with the `VMManagementImpl` and `ThreadMXBean`
    // surfaces so a reset through any of the three is visible from all of them.
    r.register(
        cls,
        "resetPeakThreadCount0",
        "()V",
        |ctx, _args| {
            reset_peak_thread_count(ctx);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // getThreads()[Ljava/lang/Thread; — REAL: enumerate the live Thread
    // objects the VM is tracking (`enumerate_threads`, the same source
    // backing `active_thread_count`). Previously returned an empty array,
    // which is a fabricated "no threads" answer for a VM that always has at
    // least the main thread alive.
    r.register(
        cls,
        "getThreads",
        "()[Ljava/lang/Thread;",
        |ctx, _args| {
            // Platform threads only (`JVM_GetAllThreads`; wave 63, lane L1).
            let threads: Vec<ObjectRef> = ctx
                .enumerate_threads(usize::MAX)
                .into_iter()
                .filter(|&t| !cratonvm_native_builtins::is_virtual_thread_object(ctx, t))
                .collect();
            let thread_cid = ctx
                .class_id_by_name("java/lang/Thread")
                .unwrap_or(ClassId::new(0));
            let arr = ctx.new_ref_array(thread_cid, threads.len());
            for (i, t) in threads.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Object(Some(*t)));            }
            Ok(Some(Value::Object(Some(arr))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // The JDK implementation stores ownership in this otherwise tiny final
    // helper. Intercepting it gives the VM a precise, JIT-independent index of
    // owned AQS synchronizers for ThreadMXBean snapshots; a heap walk would be
    // both racy and prohibitively expensive.
    // `Intrinsic`, not the ambient `Bridge`, and NOT a retired-shadow row: this triple
    // was retired once, and under `--real-jdk` a retired `Bridge` is re-tagged
    // `SyntheticStub` and dropped, so the native never ran and
    // `ThreadInfo.getLockedSynchronizers()` was always empty (`ReentrantLock`,
    // `ReentrantReadWriteLock`; Spring Boot's `ThreadDumpEndpointTests.
    // dumpThreadsAsText` expects the `Locked ownable synchronizers` rows). The
    // native is the reviewed shape for that: it performs the JDK body's exact store
    // and only adds the ownership index; `no_retired_triple_survives_the_strict_boot`
    // is why the row left the table instead of the registration hiding behind it.
    //
    // nio21 wave 2 (L4): the most-invoked native of the `HttpClient` census in
    // BOTH modes (`Nio20HttpHot -Dnio20.rows=hc`: 123 092 `--compatible`,
    // 120 179 `--real-jdk` per 200 requests, ~600 per request), and the index
    // update behind it is a sharded mutex, the thread-registry `RwLock` and a
    // per-thread `Mutex` (`ThreadRegistry::set_jmx_owned_synchronizer`), twice
    // per uncontended lock/unlock, where the JDK body is one field store the
    // JIT would inline. It is NOT retired here: with the bytecode alone
    // `ThreadInfo.getLockedSynchronizers()` is always empty, which is the
    // HotSpot divergence this registration was re-added to close. HotSpot
    // answers it with a heap walk at dump time (`ConcurrentLocksDump`), and
    // that is the fix: `docs/feature-designs/native-dispatch-architecture.md`.
    // nio21 wave 3 (L4): that fix landed -- `synchronizer_census` answers
    // `lockedSynchronizers` with one heap walk per dump -- so the registration
    // is now OFF by default (the JDK body runs and compiles in both modes) and
    // `CRATONVM_AOS_OWNER_INDEX=1` puts this intrinsic and its index back for
    // one wave as the revert.
    if aos_owner_index_enabled() {
        r.register(
            "java/util/concurrent/locks/AbstractOwnableSynchronizer",
            "setExclusiveOwnerThread",
            "(Ljava/lang/Thread;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let owner = match args.get(1) {
                    Some(Value::Object(owner)) => *owner,
                    _ => None,
                };
                // The field index is memoized per receiver class. `set_field_by_name`
                // takes the class-manager read lock and walks the hierarchy comparing
                // field-name strings on EVERY call, and this native runs twice per
                // uncontended `ReentrantLock.lock()`/`unlock()` pair — measured at
                // 821 ns per call against 7.9 ns for an ordinary Java call
                // (`probes/AqsAttributionProbe.java`, quiet host).
                //
                // The index is a per-class constant: `exclusiveOwnerThread` is the
                // only instance field `AbstractOwnableSynchronizer` declares, and
                // `resolve_field_index_in_hierarchy` walks subclass -> super, so a
                // given subclass resolves to the same slot for the life of that
                // class. The memo is keyed on the receiver's class id and holds a
                // few entries because the synchronizer classes in play are few
                // (`ReentrantLock$NonfairSync`, `$FairSync`, the read/write-lock
                // syncs, `ThreadPoolExecutor$Worker`) — a one-entry cache would
                // thrash between a lock and a pool worker.
                //
                // A miss falls back to the resolving path, so an unexpected layout
                // is slow rather than wrong, and an unknown name stays the silent
                // no-op `set_field_by_name` already was. Class redefinition needs no
                // invalidation: it allocates a NEW `ClassId`, so a stale entry can
                // never be consulted for the redefined class.
                //
                // gc-common w18-g: keyed on (VM identity, class id). `ClassId`s
                // are minted per VM from zero, and one OS thread can run natives
                // for more than one VM (an embedder, the `vm` test binary), so a
                // memo keyed on the class id alone could hand VM B the slot of
                // VM A's class with the same id -- and store the owner `Thread`
                // into an unrelated field of VM B's object.
                const MEMO_SLOTS: usize = 8;
                thread_local! {
                    static OWNER_FIELD_INDEX: std::cell::RefCell<[(usize, u32, u32); MEMO_SLOTS]> =
                        const { std::cell::RefCell::new([(0, u32::MAX, 0); MEMO_SLOTS]) };
                }
                let vm = ctx.vm_identity();
                let class_id = ctx.class_id_of_object(this);
                let raw_cid = class_id.as_u32();
                let cached = OWNER_FIELD_INDEX.with(|memo| {
                    memo.borrow()
                        .iter()
                        .find(|(memo_vm, cid, _)| *cid == raw_cid && *memo_vm == vm)
                        .map(|(_, _, index)| *index as usize)
                });
                match cached {
                    Some(index) => ctx.set_field(this, index, Value::Object(owner)),
                    None => {
                        if let Some(index) =
                            ctx.resolve_field_index_by_class_id(class_id, "exclusiveOwnerThread")
                        {
                            OWNER_FIELD_INDEX.with(|memo| {
                                let mut memo = memo.borrow_mut();
                                // Take a free slot, else evict slot 0. The policy does
                                // not need to be clever at this size, but the table
                                // must not be able to grow without bound.
                                let victim = memo
                                    .iter()
                                    .position(|(_, cid, _)| *cid == u32::MAX)
                                    .unwrap_or(0);
                                memo[victim] = (vm, raw_cid, index as u32);
                            });
                            ctx.set_field(this, index, Value::Object(owner));
                        }
                    }
                }
                ctx.record_jmx_owned_synchronizer(this, owner);
                Ok(None)
            },
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }

    // REAL(detector): both walk the wait-for graph built from the VM's own
    // per-thread JMX snapshots and report the threads on a cycle — see
    // `deadlocked_thread_ids`, which now observes real edges (the contended
    // monitor-enter path publishes them). `null` still means "no deadlock", per
    // the JMM.
    //
    // Both names share one detector because the snapshot carries no lock-KIND
    // discriminator, so monitor-only deadlocks cannot yet be separated from
    // ownable-synchronizer ones; when `ThreadJmxSnapshot::lock` starts being
    // populated the monitor-only variant can filter on it.
    //
    // DESCRIPTOR FIX: JDK 25 declares these as `()[Ljava/lang/Thread;`, not
    // `()[J` (verified with `javap`; the Java side then calls
    // `threadsToIds(Thread[])`). The `[J` registrations could therefore never
    // dispatch on this class — harmless while `ThreadImpl.findDeadlockedThreads()`
    // threw UnsupportedOperationException up front, but it gates on
    // `isSynchronizerUsageSupported()`, which now answers true, so the call
    // reaches the native and an unregistered one would be an
    // UnsatisfiedLinkError. Both shapes are registered; the `[J` pair is kept
    // for any JDK that declares the older form.
    r.register(
        cls,
        "findMonitorDeadlockedThreads0",
        "()[Ljava/lang/Thread;",
        |ctx, _args| deadlocked_threads_object_result(ctx, false),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "findDeadlockedThreads0",
        "()[Ljava/lang/Thread;",
        |ctx, _args| deadlocked_threads_object_result(ctx, true),
        cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `CRATONVM_AOS_OWNER_INDEX=1`: register the
/// `AbstractOwnableSynchronizer.setExclusiveOwnerThread` intrinsic and its
/// per-lock owner index again (the pre-nio21-wave-3 answer). Default OFF since
/// nio21 wave 3 (lane L4): the JDK's one-store body runs (and is inlined by the
/// JIT) in both modes, and `ThreadInfo.getLockedSynchronizers()` is answered by
/// the dump-time heap walk [`synchronizer_census`] -- HotSpot's
/// `ConcurrentLocksDump` -- instead of by the index
/// (`r21-aqs-owner-index-is-a-native-call-per-lock-FIXED-20261008.md`). Kept one wave
/// as the revert. Read once per registry population (boot) and per dump, so no
/// cache. nio21 wave 2, L4; default flipped wave 3, L4.
pub(in crate::jmx) fn aos_owner_index_enabled() -> bool {
    false}

/// nio21 wave 3, lane L4: the dump-time owner census, taken once per dump
/// door that reports `lockedSynchronizers`, when the per-lock index is off:
/// `(owner's Java tid, synchronizer)` in heap order. The references are raw
/// (`owned_synchronizer_census`'s are valid only until the next allocation),
/// so the caller roots them before it allocates: [`synchronizer_census`] pins
/// them, the `NativeHandleScope` door roots them in its scope.
///
/// `None` -- the index is on (it answers through the snapshot, as before), or
/// the VM could not walk -- leaves every snapshot's own list alone.
pub(in crate::jmx) fn synchronizer_census_rows(ctx: &mut dyn NativeContext) -> Option<Vec<(i64, ObjectRef)>> {
    if aos_owner_index_enabled() {
        return None;
    }
    let pairs = ctx.owned_synchronizer_census()?;
    // A field read allocates nothing, so the pairs stay valid across it.
    Some(
        pairs
            .into_iter()
            .filter_map(|(owner, synchronizer)| {
                match cratonvm_native_builtins::lang_misc::thread_field(&*ctx, owner, "tid") {
                    Value::Long(id) => Some((id, synchronizer)),
                    Value::Int(id) => Some((i64::from(id), synchronizer)),
                    _ => None,
                }
            })
            .collect(),
    )
}

/// [`synchronizer_census_rows`] with each synchronizer pinned at once, as
/// `(owner tid, pin handle, synchronizer as pinned)`. Taken AFTER the caller's
/// own pins, so the caller's `unpin_native_roots` releases these too.
pub(in crate::jmx) fn synchronizer_census(ctx: &mut dyn NativeContext) -> Option<Vec<(i64, usize, ObjectRef)>> {
    let rows = synchronizer_census_rows(ctx)?;
    Some(
        rows.into_iter()
            .map(|(tid, synchronizer)| (tid, ctx.pin_native_root(synchronizer), synchronizer))
            .collect(),
    )
}

/// The census rows owned by the thread whose Java tid is `thread_id`, read
/// back through their pins (current addresses), in heap order: HotSpot's
/// `ThreadConcurrentLocks` list for that thread.
pub(in crate::jmx) fn census_synchronizers_of(
    ctx: &dyn NativeContext,
    census: &[(i64, usize, ObjectRef)],
    thread_id: i64,
) -> Vec<ObjectRef> {
    census
        .iter()
        .filter(|(tid, _, _)| *tid == thread_id)
        .map(|&(_, pin, fallback)| ctx.read_native_pin(pin, fallback))
        .collect()
}

/// `sun.management.ClassLoadingImpl` — most info comes via
/// `ManagementFactoryHelper`, so this is intentionally minimal.
pub fn register_class_loading_impl(r: &mut NativeMethodRegistry) {
            let cls = "sun/management/ClassLoadingImpl";

    // setVerboseClass(Z)V — the write side of the `-verbose:class` round-trip.
    // The real `ClassLoadingImpl.isVerbose()` bytecode reads it straight back
    // via `jvm.getVerboseClass()`, so accepting and ignoring made
    // `ClassLoadingMXBean.setVerbose(b)` unobservable — the JMM contract says
    // it must not be. CratonVM has no class-load tracing to switch on, so the
    // flag is state-only; see `JmmSwitches::verbose_class`.
    r.register(
        cls,
        "setVerboseClass",
        "(Z)V",
        |ctx, args| {
            let on = bool_flag_arg(args);
            jmm_switch_set(ctx, |s| s.verbose_class = on);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // <init>(Lsun/management/VMManagement;)V — mirror the real
    // `ClassLoadingImpl(VMManagement vm) { this.jvm = vm; }`. Dropping the
    // argument left `jvm` null on every instance, so each inherited bytecode
    // accessor that routes through it (`getLoadedClassCount`, `isVerbose`,
    // `setVerboseClass`) NPE'd on `this.jvm` the moment a JMX client touched
    // the bean — the same shape as the `MemoryImpl.isVerbose` null-`jvm` bug
    // documented in `register_jmx_natives`.
    r.register(
        cls,
        "<init>",
        "(Lsun/management/VMManagement;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let jvm = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "jvm", jvm);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// Establish the state `MemoryManagerImpl`'s Java constructor establishes:
/// the manager's `name` (which `getName()` reads straight back) and the
/// `isValid` flag `ManagementFactoryHelper` filters the platform bean lists
/// on. Shared by the `MemoryManagerImpl` / `GarbageCollectorImpl`
/// constructors and by the `alloc_*` factory paths, so a bean carries the
/// same state whichever way it was built.
pub(in crate::jmx) fn init_memory_manager_fields(ctx: &mut dyn NativeContext, this: ObjectRef, name: Value) {
    ctx.set_field_by_name(this, "name", name);
    ctx.set_field_by_name(this, "isValid", Value::Int(1));
}

/// `MemoryManagerImpl.isValid()` / `MemoryPoolImpl.isValid()`.
///
/// Reads the per-instance flag the constructors above establish
/// (`init_memory_manager_fields`, `MemoryPoolImpl.<init>`), defaulting to
/// `true` for a receiver whose layout carries no such field. The previous
/// blanket `Ok(Some(Value::Int(1)))` made that stored flag dead state: a bean
/// could never report itself invalid, so `ManagementFactoryHelper`'s validity
/// filter had nothing to filter on.
pub(in crate::jmx) fn native_manager_is_valid(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(match ctx.get_field_by_name(this, "isValid") {
        Value::Int(v) => Value::Int(i32::from(v != 0)),
        _ => Value::Int(1),
    }))
}

/// Read a `long` constructor/method argument. Callers reaching a native
/// through a JIT'd or reflective path can present a `long` as `Value::Int`,
/// so accept both rather than silently defaulting a real argument to 0.
pub(in crate::jmx) fn long_arg(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

/// The JMM's "metric unavailable" `MemoryUsage` (-1, -1, -1, -1).
///
/// The answer for a pool bean the running collector does not describe
/// (`NativeContext::gc_memory_pools` empty or without that name: mocks and
/// the historical `Eden Space` / `Old Gen` pair). Every backend's own pools
/// answer real figures (`gc_pool_usage`); gci i14/mp corrected this doc, which
/// still said no backend had per-pool accounting.
pub(in crate::jmx) fn undefined_memory_usage(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let mu = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4)?;
    // By NAME -- see `memory_usage_slots`. The value is the same in all four,
    // so this one is a no-op on behaviour twice over; it is converted so the
    // bean has no remaining index-into-a-real-layout site to audit.
    for slot in memory_usage_slots(ctx, mu) {
        ctx.set_field(mu, slot, Value::Long(-1));
    }
    Ok(mu)
}

