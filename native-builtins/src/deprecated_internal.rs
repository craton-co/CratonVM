// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deprecated java.beans, java.rmi, sun.misc, and jdk.internal.* native implementations.
//!
//! These APIs are deprecated or removed in modern JDKs but legacy code may still call
//! them. Every method is registered via NativeMethodRegistry with real implementations
//! (not stubs): proper validation, error handling, and security checks.

use crate::lang_class::ClassTextName;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ===========================================================================
// Off-heap memory tracking (T8.4.2)
//
// SECURITY FIX (V5): this *tracked* store is no longer wired to any native —
// `register_unsafe_deprecated_natives` now routes off-heap allocate/realloc/
// free/setMemory/copyMemory through the single arena store
// (`crate::unsafe_natives::register_consolidated_off_heap_store`). The code is
// retained (not deleted) per the V5 directive but is dead; the
// `#[allow(dead_code)]` attributes below keep the build warning-clean without
// removing a store whose deletion would be risky.
// ===========================================================================

/// Global counter for generating unique memory addresses.
#[allow(dead_code)]
static NEXT_MEM_ADDR: AtomicU64 = AtomicU64::new(0x1_0000_0000); // start above 4GB

/// Tracked off-heap memory blocks: address -> Vec<u8>.
#[allow(dead_code)]
fn off_heap_store() -> &'static Mutex<HashMap<u64, Vec<u8>>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<u64, Vec<u8>>>> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(dead_code)]
fn tracked_allocate(size: usize) -> u64 {
    let addr = NEXT_MEM_ADDR.fetch_add(size as u64 + 64, Ordering::Relaxed);
    let block = vec![0u8; size];
    off_heap_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(addr, block);
    addr
}

#[allow(dead_code)]
fn tracked_free(addr: u64) -> bool {
    off_heap_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr)
        .is_some()
}

#[allow(dead_code)]
fn tracked_realloc(old_addr: u64, new_size: usize) -> Result<u64, &'static str> {
    let mut store = off_heap_store().lock().unwrap_or_else(|e| e.into_inner());
    let old_block = store
        .remove(&old_addr)
        .ok_or("invalid address for realloc")?;
    let new_addr = NEXT_MEM_ADDR.fetch_add(new_size as u64 + 64, Ordering::Relaxed);
    let mut new_block = vec![0u8; new_size];
    let copy_len = old_block.len().min(new_size);
    new_block[..copy_len].copy_from_slice(&old_block[..copy_len]);
    store.insert(new_addr, new_block);
    Ok(new_addr)
}

#[allow(dead_code)]
fn tracked_set_memory(addr: u64, offset: usize, count: usize, value: u8) -> bool {
    let mut store = off_heap_store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(block) = store.get_mut(&addr) {
        let end = offset.saturating_add(count);
        if end <= block.len() {
            for b in &mut block[offset..end] {
                *b = value;
            }
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn tracked_copy_memory(
    src_addr: u64,
    src_offset: usize,
    dst_addr: u64,
    dst_offset: usize,
    count: usize,
) -> bool {
    let mut store = off_heap_store().lock().unwrap_or_else(|e| e.into_inner());
    // Need to handle same-block copy
    if src_addr == dst_addr {
        if let Some(block) = store.get_mut(&src_addr) {
            let src_end = src_offset.saturating_add(count);
            let dst_end = dst_offset.saturating_add(count);
            if src_end <= block.len() && dst_end <= block.len() {
                block.copy_within(src_offset..src_end, dst_offset);
                return true;
            }
        }
        return false;
    }
    // Two distinct blocks: read src first, then write dst
    let src_data = {
        let src_block = match store.get(&src_addr) {
            Some(b) => b,
            None => return false,
        };
        let src_end = src_offset.saturating_add(count);
        if src_end > src_block.len() {
            return false;
        }
        src_block[src_offset..src_end].to_vec()
    };
    if let Some(dst_block) = store.get_mut(&dst_addr) {
        let dst_end = dst_offset.saturating_add(count);
        if dst_end > dst_block.len() {
            return false;
        }
        dst_block[dst_offset..dst_end].copy_from_slice(&src_data);
        true
    } else {
        false
    }
}

#[allow(dead_code)]
fn tracked_read(addr: u64, offset: usize, count: usize) -> Option<Vec<u8>> {
    let store = off_heap_store().lock().unwrap_or_else(|e| e.into_inner());
    store.get(&addr).and_then(|block| {
        let end = offset.saturating_add(count);
        if end <= block.len() {
            Some(block[offset..end].to_vec())
        } else {
            None
        }
    })
}

// ===========================================================================
// Signal handler tracking (T8.4.4)
// ===========================================================================

/// Signal number -> the handler `Signal.handle` installed (`None` for
/// SIG_DFL), ONE TABLE PER VM.
///
/// gc-common w10-a (`common-w8a-process-global-object-singletons-in-native-builtins`):
///
/// * **Per VM.** This was one process-wide map, so `Signal.raise` in VM B
///   called VM A's handler at an address in A's heap, and B's `handle`
///   returned A's handler as "the previous one". Rows are dropped by
///   [`forget_vm_signal_handlers`] at VM teardown.
/// * **Rooted and remapped.** The handlers were bare `ObjectRef`s, neither GC
///   roots nor remapped (the old KNOWN-UNSOUND note here): `Signal.raise`
///   after a moving collection invoked a stale or reclaimed handler. They are
///   now `(global_root, ObjectRef)` pairs: a JNI global root of the owning VM,
///   resolved at every use, and released when `handle` replaces the handler;
///   the raw copy is the answer only in a context with no global roots
///   (handle 0: the mocks, which never move objects).
/// * **gc-common w29-d.** The root used to be `register_var_handle_root`
///   with an identity-hash key re-read through `read_var_handle_root`. That
///   registry is keyed by identity hash, so `raise` could invoke whichever
///   object had been registered FIRST under the handler's hash (a VarHandle,
///   a TypeVariable, another VM-internal singleton), and every handler ever
///   installed stayed rooted for the VM's life.
static SIGNAL_HANDLERS: cratonvm_native_api::vm_scoped::VmScoped<
    HashMap<i32, Option<(usize, ObjectRef)>>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// The CURRENT address of a `(global_root, raw)` handler row.
fn resolve_signal_handler(ctx: &dyn NativeContext, row: (usize, ObjectRef)) -> ObjectRef {
    let (root, cached) = row;
    if root == 0 {
        return cached;
    }
    ctx.resolve_global_root(root).unwrap_or(cached)
}

/// Drop `vm`'s signal handlers (VM teardown, from
/// `lib.rs::forget_vm_native_object_singletons`). Idempotent.
pub fn forget_vm_signal_handlers(vm: usize) {
    SIGNAL_HANDLERS.forget(vm);
    SIGNAL_DISPOSITIONS.forget(vm);
}

/// Signal number -> the disposition `jdk.internal.misc.Signal.handle0` last
/// installed, ONE TABLE PER VM (the rule `SIGNAL_HANDLERS` above states):
/// `0` SIG_DFL, `1` SIG_IGN, `2` "the Java handler" (`os::user_handler()`),
/// anything else a native handler address a `NativeHandler` carried back in.
/// A signal with no row is at SIG_DFL.
///
/// SBR wave 5 FJP2. `handle0` used to answer `0` unconditionally, so a
/// second `Signal.handle` of the same signal was told the previous handler was
/// SIG_DFL -- HotSpot's `JVM_RegisterSignal` answers `2` there and the JDK
/// hands back the handler it displaced -- and `raise0` was a no-op, so the real
/// `Signal.raise` (what `sun.misc.Signal.raise` reaches once its shadows are
/// retired) ran nothing. This per-VM table supplies the Java-visible disposition;
/// the launcher separately owns OS delivery on an attached service thread.
static SIGNAL_DISPOSITIONS: cratonvm_native_api::vm_scoped::VmScoped<HashMap<i32, i64>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Signals HotSpot's `JVM_RegisterSignal` refuses on Linux (`-1`, which the JDK
/// turns into `IllegalArgumentException("Signal already used by VM or OS: ..")`):
/// the synchronous-fault signals it owns (SEGV, FPE, ILL) and `BREAK_SIGNAL`
/// (QUIT, the thread dump). Measured on HotSpot 25 by
/// `tools/probes/SbrW5Fjp2.java` (`sig handle QUIT/SEGV/FPE/ILL`); BUS, PIPE
/// and USR2 are NOT refused there (it answers its own native handler).
///
/// Windows (interpreter round i5 wave 100, lane L1; page
/// `interpreter-i100-L1-windows-signal-names-and-numbers-are-linuxs-FIXED-20261015.md`):
/// `jvm_windows.cpp`'s `JVM_RegisterSignal` refuses `SIGFPE` (8) and
/// `SIGBREAK` (21, the thread-dump console break, refused with or without
/// `-Xrs`); `SIGSEGV` / `SIGILL` go to the CRT's `signal()` like any other.
/// The numbers are the MSVC ones `findSignal0` answers there.
fn signal_reserved_by_vm(sig: i32) -> bool {
    if cfg!(windows) {
        return matches!(sig, 8 | 21);
    }
    // KILL (9) and STOP (19): not the VM's, but `os::signal`'s `sigaction`
    // fails for them, and HotSpot answers that failure as `-1` too (wave
    // 100, lane L1: they were accepted and answered SIG_DFL).
    matches!(sig, 3 | 4 | 8 | 9 | 11 | 19)
}

/// The shutdown-hook signals of `JVM_RegisterSignal` (`SHUTDOWN*_SIGNAL`):
/// refused under `-Xrs`, and an inherited ignore sticks. HUP, INT, TERM on
/// POSIX; INT and TERM on Windows, which has no HUP (wave 100, lane L1).
fn shutdown_signal(sig: i32) -> bool {
    if cfg!(windows) {
        return matches!(sig, 2 | 15);
    }
    matches!(sig, 1 | 2 | 15)
}

/// The disposition of `sig` before any `handle0` in this VM: what HotSpot's
/// `install_generic_signal_handler` answers, the process's CURRENT OS
/// disposition -- `0` SIG_DFL, `1` SIG_IGN, else the installed handler's
/// address (a handler this process -- the crash handler, the Rust runtime's
/// SIGPIPE ignore -- put there). Only read, never changed. `0` off Unix.
fn signal_os_disposition(sig: i32) -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: a query-only `sigaction` (null new action) into a zeroed
        // out-parameter; no handler is installed or changed.
        let handler = unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
                return 0;
            }
            old.sa_sigaction
        };
        if handler == libc::SIG_DFL {
            0
        } else if handler == libc::SIG_IGN {
            1
        } else {
            handler as i64
        }
    }
    #[cfg(not(unix))]
    {
        let _ = sig;
        0
    }
}

/// `jdk.internal.misc.Signal.handle0(int sig, long nativeH)`: install `nativeH`
/// and answer the PREVIOUS disposition, per `SIGNAL_DISPOSITIONS`.
pub(crate) fn native_signal_handle0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sig = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    let new_h = match args.get(1) {
        Some(Value::Long(h)) => *h,
        Some(Value::Int(h)) => i64::from(*h),
        _ => 0,
    };
    if sig <= 0 || signal_reserved_by_vm(sig) {
        return Ok(Some(Value::Long(-1)));
    }
    let shutdown_signal = shutdown_signal(sig);
    if shutdown_signal && ctx.reduce_signal_usage() {
        return Ok(Some(Value::Long(-1)));
    }
    // HotSpot never changes an already ignored shutdown signal. This applies
    // to inherited SIG_IGN as well as a previous Java request, so returning a
    // displaced handler and reinstalling it must not silently undo the ignore.
    let os_ignored = shutdown_signal && signal_os_disposition(sig) == 1;
    // POSIX only: `jvm_windows.cpp` has no `is_sig_ignored` test (wave 100).
    let ignore_sticks = shutdown_signal && !cfg!(windows);
    let old = SIGNAL_DISPOSITIONS
        .with(ctx.vm_identity(), |table| {
            if ignore_sticks && (os_ignored || table.get(&sig) == Some(&1)) {
                table.insert(sig, 1);
                Some(1)
            } else {
                table.insert(sig, new_h)
            }
        })
        .unwrap_or_else(|| hotspot_signal_disposition(sig));
    Ok(Some(Value::Long(old)))
}

/// The address a `NativeHandler` carries for a signal HotSpot installs its own
/// native handler for at start-up. The value is only ever handed back to
/// `handle0` (it is never called), so any address that is not `0`, `1` or `2`
/// does. SBR round 5, lane compat2.
const HOTSPOT_INSTALLED_HANDLER: i64 = 0x5CA7_0000;

/// What HotSpot's `JVM_RegisterSignal` answers for `sig` on a fresh VM:
/// [`signal_os_disposition`], except for the signals the JVM puts its own native
/// handler on at start-up (SIGBUS, SIGUSR2 -- the suspend/resume signal --,
/// SIGPIPE, SIGXFSZ), where this VM's process has the default or ignore
/// disposition and `Signal.handle` would answer `SIG_DFL` / `SIG_IGN` instead of
/// the `NativeHandler` that HotSpot's answer becomes (measured on HotSpot 25:
/// `SbrW5Fjp2` `sig handle USR2` / `PIPE` print `sun.misc.Signal$SunMiscHandler`).
fn hotspot_signal_disposition(sig: i32) -> i64 {
    let os = signal_os_disposition(sig);
    // Interpreter round i5 wave 100 hand-back (lane L1, `L1W100SignalNamesAndNumbers`
    // on Linux): SIGABRT (6) carries THIS VM's crash handler
    // (`runtime::crash_handler`, its `CRASH_SIGNALS`), which HotSpot does not
    // install -- HotSpot 25 answers `SIG_DFL` there. The other crash signals
    // are refused (`signal_reserved_by_vm`) or answered as HotSpot's own
    // handler (BUS) above, so ABRT is the one the crash handler leaked into.
    if cfg!(unix) && sig == 6 && os > 1 {
        return 0;
    }
    if os <= 1 && matches!(sig, 7 | 12 | 13 | 25) {
        HOTSPOT_INSTALLED_HANDLER + i64::from(sig)
    } else {
        os
    }
}

/// `jdk.internal.misc.Signal.raise0(int sig)`. HotSpot raises the host signal,
/// its handler posts the number to the "Signal Dispatcher" thread, and that
/// thread calls `Signal.dispatch(number)`, which starts a `SIG<name> handler`
/// thread running the Java handler. With a Java handler installed
/// (disposition `2`) this calls `dispatch` directly -- the same Java, minus the
/// host round trip; `raise` only reaches here after checking the JDK's own
/// `handlers` table. Without one it stays a no-op: raising the host signal would
/// take the OS default action (terminate) on a process that installed nothing.
pub(crate) fn native_signal_raise0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sig = args.first().and_then(|v| v.as_int()).unwrap_or(0);
    let java_handler = SIGNAL_DISPOSITIONS
        .peek(ctx.vm_identity(), |table| table.get(&sig).copied())
        .flatten()
        == Some(2);
    if java_handler {
        ctx.invoke(
            "jdk/internal/misc/Signal",
            "dispatch",
            "(I)V",
            &[Value::Int(sig)],
        )?;
    }
    Ok(None)
}

/// The launcher distinguishes an explicit Java SIG_DFL from its startup
/// shutdown policy, and SIG_IGN from a missing Java callback. State is per VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSignalDisposition {
    Shutdown,
    Default,
    Ignore,
    Java,
}

pub fn host_signal_disposition(ctx: &dyn NativeContext, sig: i32) -> HostSignalDisposition {
    host_signal_disposition_for_vm(ctx.vm_identity(), sig)
}

/// [`host_signal_disposition`] by the VM's identity, for a caller that has
/// no context: zt t27/fin1's launcher exit backstop
/// (`vm/src/runtime/launcher_signals.rs`), which runs on a thread that never
/// attached to the VM.
pub fn host_signal_disposition_for_vm(vm_identity: usize, sig: i32) -> HostSignalDisposition {
    match SIGNAL_DISPOSITIONS
        .peek(vm_identity, |table| table.get(&sig).copied())
        .flatten()
    {
        Some(0) => HostSignalDisposition::Default,
        Some(1) => HostSignalDisposition::Ignore,
        Some(2) => HostSignalDisposition::Java,
        _ => HostSignalDisposition::Shutdown,
    }
}

/// Deliver a launcher-caught host signal to this VM's Java handler, if one
/// was registered. The launcher checks host_signal_disposition first.
pub fn dispatch_host_signal(ctx: &mut dyn NativeContext, sig: i32) -> bool {
    let handled = SIGNAL_DISPOSITIONS
        .peek(ctx.vm_identity(), |table| table.get(&sig).copied())
        .flatten()
        == Some(2);
    if handled {
        if let Err(error) = ctx.invoke(
            "jdk/internal/misc/Signal",
            "dispatch",
            "(I)V",
            &[Value::Int(sig)],
        ) {
            tracing::error!("Java signal dispatch failed: {error}");
        }
    }
    handled
}

/// The descriptor of `SignalHandler.handle` for `signal_obj`'s own class:
/// `jdk.internal.misc.SignalHandler.handle(jdk.internal.misc.Signal)` or
/// `sun.misc.SignalHandler.handle(sun.misc.Signal)`.
///
/// gc-common w10-a: `raise` used to choose by `sig_num >= 0`, which is always
/// true, so a `jdk.internal.misc.Signal` handler was invoked with the
/// `sun.misc` descriptor, resolved nothing, and was silently never called.
fn signal_handler_descriptor(ctx: &dyn NativeContext, signal_obj: ObjectRef) -> &'static str {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(signal_obj))
        .unwrap_or_default();
    if class_name.starts_with("jdk/internal/misc/") {
        "(Ljdk/internal/misc/Signal;)V"
    } else {
        "(Lsun/misc/Signal;)V"
    }
}

/// Map signal name -> number.
fn signal_name_to_number(name: &str) -> Option<i32> {
    match name.to_uppercase().as_str() {
        "HUP" | "SIGHUP" => Some(1),
        "INT" | "SIGINT" => Some(2),
        "QUIT" | "SIGQUIT" => Some(3),
        "ILL" | "SIGILL" => Some(4),
        "TRAP" | "SIGTRAP" => Some(5),
        "ABRT" | "SIGABRT" | "IOT" => Some(6),
        "BUS" | "SIGBUS" => Some(7),
        "FPE" | "SIGFPE" => Some(8),
        "KILL" | "SIGKILL" => Some(9),
        "USR1" | "SIGUSR1" => Some(10),
        "SEGV" | "SIGSEGV" => Some(11),
        "USR2" | "SIGUSR2" => Some(12),
        "PIPE" | "SIGPIPE" => Some(13),
        "ALRM" | "SIGALRM" => Some(14),
        "TERM" | "SIGTERM" => Some(15),
        _ => None,
    }
}

fn signal_number_to_name(num: i32) -> &'static str {
    match num {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        4 => "ILL",
        5 => "TRAP",
        6 => "ABRT",
        7 => "BUS",
        8 => "FPE",
        9 => "KILL",
        10 => "USR1",
        11 => "SEGV",
        12 => "USR2",
        13 => "PIPE",
        14 => "ALRM",
        15 => "TERM",
        _ => "UNKNOWN",
    }
}

// ===========================================================================
// Registration
// ===========================================================================

pub fn register_deprecated_internal_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_beans_natives(r);
    register_rmi_natives(r);
    register_activation_natives(r);
    register_unsafe_deprecated_natives(r);
    register_reflection_natives(r);
    register_signal_natives(r);
    r.set_category(__prev_cat);
}

// ===========================================================================
// T8.3.1 — java.beans.Beans
// ===========================================================================

fn register_beans_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let beans = "java/beans/Beans";

    // instantiate(ClassLoader, String) -> Object
    r.register(
        beans,
        "instantiate",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            // args[0] = ClassLoader (or null), args[1] = bean class name
            let bean_name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::ClassNotFoundException {
                        class_name: "<null>".to_string(),
                    }
                    .into());
                }
            };

            let bean_name = ctx.read_string(bean_name_obj).unwrap_or_default();
            if bean_name.is_empty() {
                return Err(RuntimeError::ClassNotFoundException {
                    class_name: "<empty>".to_string(),
                }
                .into());
            }

            // Convert dots to slashes for internal class name
            let internal_name = bean_name.replace('.', "/");

            // Ensure the class is loaded and initialized
            let class_id = match ctx.ensure_class_initialized(&internal_name) {
                Ok(cid) => cid,
                Err(_) => {
                    return Err(RuntimeError::ClassNotFoundException {
                        class_name: bean_name,
                    }
                    .into());
                }
            };

            // Allocate and return a new instance (simulates no-arg constructor)
            let obj = ctx.alloc_object(class_id, 4);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // isDesignTime() / isGuiAvailable() — now read real, mutable state.
    //
    // The previous justification for the constants ("their only mutators are
    // `setDesignTime`/`setGuiAvailable`, which this module does not register at
    // all, so no caller can put the VM into a state either would misreport")
    // had the shadowing rule backwards. NOT registering the setters means the
    // setters' REAL bytecode runs and updates `ThreadGroupContext`, while these
    // getters — which DO shadow the real bytecode — keep answering the
    // constant. Setting design time then silently had no effect.
    //
    // Both accessor pairs are therefore registered together and backed by the
    // statics below, so the getter observes the setter in both run modes.
    // `isGuiAvailable`'s un-set default follows the real JDK
    // (`!GraphicsEnvironment.isHeadless()`) rather than a hard `false`;
    // `system_bootstrap` seeds `java.awt.headless=true`, so an ordinary run
    // still answers false, but `-Djava.awt.headless=false` is now honoured.
    r.register(beans, "isDesignTime", "()Z", native_beans_is_design_time);
    r.register(beans, "setDesignTime", "(Z)V", native_beans_set_design_time);
    r.register(
        beans,
        "isGuiAvailable",
        "()Z",
        native_beans_is_gui_available,
    );
    r.register(
        beans,
        "setGuiAvailable",
        "(Z)V",
        native_beans_set_gui_available,
    );
    r.set_category(__prev_cat);
}

/// `java.beans.Beans` state. The real JDK scopes it per `ThreadGroupContext`;
/// CratonVM has one such context per VM in practice, so one row per VM is
/// observationally equivalent.
///
/// gc-common w10-a: these were two process-wide atomics, so VM A's
/// `Beans.setDesignTime(true)` made VM B's `Beans.isDesignTime()` answer true.
/// Rows are dropped by
/// [`forget_vm_beans_state`] at VM teardown.
#[derive(Clone, Copy)]
struct BeansState {
    /// The design-time flag.
    design_time: bool,
    /// The GUI-available override: `-1` un-set (fall back to the headless
    /// computation), `0` false, `1` true. Mirrors the real JDK's
    /// `ThreadGroupContext.isGuiAvailable` being a nullable `Boolean`.
    gui_available: i8,
}

impl Default for BeansState {
    fn default() -> Self {
        Self {
            design_time: false,
            gui_available: -1,
        }
    }
}

static BEANS_STATE: cratonvm_native_api::vm_scoped::VmScoped<BeansState> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Drop `vm`'s `java.beans.Beans` state (VM teardown). Idempotent.
pub fn forget_vm_beans_state(vm: usize) {
    BEANS_STATE.forget(vm);
}

fn beans_state(ctx: &dyn NativeContext) -> BeansState {
    BEANS_STATE
        .peek(ctx.vm_identity(), |s| *s)
        .unwrap_or_default()
}

fn native_beans_is_design_time(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let on = beans_state(ctx).design_time;
    Ok(Some(Value::Int(i32::from(on))))
}

fn native_beans_set_design_time(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static (Z)V — args[0] is the boolean.
    let on = matches!(args.first(), Some(Value::Int(v)) if *v != 0);
    BEANS_STATE.with(ctx.vm_identity(), |s| s.design_time = on);
    Ok(None)
}

fn native_beans_is_gui_available(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let available = match beans_state(ctx).gui_available {
        0 => false,
        1 => true,
        // Un-set: real JDK answers `!GraphicsEnvironment.isHeadless()`. Only an
        // explicit `java.awt.headless=false` makes this VM non-headless; unset
        // defaults to headless, matching `native-awt`'s GraphicsEnvironment
        // natives.
        _ => ctx
            .get_system_property("java.awt.headless")
            .is_some_and(|v| v.eq_ignore_ascii_case("false")),
    };
    Ok(Some(Value::Int(i32::from(available))))
}

fn native_beans_set_gui_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let on = matches!(args.first(), Some(Value::Int(v)) if *v != 0);
    BEANS_STATE.with(ctx.vm_identity(), |s| s.gui_available = i8::from(on));
    Ok(None)
}

// ===========================================================================
// T8.3.2 — java.rmi.server.RemoteRef
// ===========================================================================

fn register_rmi_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let remote_ref = "java/rmi/server/RemoteRef";

    // getRefClass(ObjectOutput) -> String
    r.register(
        remote_ref,
        "getRefClass",
        "(Ljava/io/ObjectOutput;)Ljava/lang/String;",
        |ctx, _args| {
            // Deprecated — return empty string per spec
            let empty = ctx.create_string("");
            Ok(Some(Value::Object(Some(empty))))
        },
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// T8.3.3 — java.rmi.activation.*
// ===========================================================================

fn register_activation_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Activatable.<init>()V — genuinely empty; see the note on
    // `ActivationGroup.<init>` below for why every `<init>` in this module is a
    // real no-op rather than a suppressed body.
    r.register(
        "java/rmi/activation/Activatable",
        "<init>",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // Activatable.register(ActivationDesc) -> ActivationID
    // Throws ActivationException since activation was removed in JDK 17
    r.register(
        "java/rmi/activation/Activatable",
        "register",
        "(Ljava/rmi/activation/ActivationDesc;)Ljava/rmi/activation/ActivationID;",
        activation_throws,
    );

    // Activatable.exportObject(Remote, ActivationID, int) -> Remote
    r.register(
        "java/rmi/activation/Activatable",
        "exportObject",
        "(Ljava/rmi/Remote;Ljava/rmi/activation/ActivationID;I)Ljava/rmi/Remote;",
        activation_throws,
    );

    // ActivationGroup.getSystem() -> ActivationSystem
    //
    // Was a constant null, which contradicted every other method in this
    // module: `register`/`exportObject` above tell the caller plainly that
    // `java.rmi.activation` was removed in JDK 17, while `getSystem()` handed
    // back a null that the caller could only discover by NPE-ing on it several
    // frames later.  Real `getSystem()` throws when no system is set, so
    // throwing here is BOTH spec-shaped and the honest answer.
    r.register(
        "java/rmi/activation/ActivationGroup",
        "getSystem",
        "()Ljava/rmi/activation/ActivationSystem;",
        activation_throws,
    );

    // The two `<init>()V` no-ops below are genuinely empty: `java.rmi
    // .activation` was removed in JDK 17, so in real-JDK mode these classes do
    // not exist at all and the registrations are inert, while in synthetic-JDK
    // mode the stub class has no state to initialise. They exist only so a
    // `new` reaches the caller's next call — which is one of the throwing
    // methods above, i.e. the point at which the caller learns the truth.
    r.register(
        "java/rmi/activation/ActivationGroup",
        "<init>",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // `java/rmi/activation/ActivationSystem.<init>()V` REMOVED: `ActivationSystem`
    // is an INTERFACE, so no bytecode can ever contain `new ActivationSystem()`
    // / `invokespecial ActivationSystem.<init>` — the registration was
    // unreachable in both run modes, and it is not in
    // `deprecated_verify`'s manifest or checklist.
    r.set_category(__prev_cat);
}

/// Common handler for activation methods that should throw when called.
fn activation_throws(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(RuntimeError::NotImplemented {
        feature: "java.rmi.activation was removed in JDK 17. Activation is no longer supported."
            .to_string(),
    }
    .into())
}

// ===========================================================================
// T8.4.1 + T8.4.2 — sun.misc.Unsafe deprecated operations
// ===========================================================================

fn register_unsafe_deprecated_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- T8.4.1: defineClass ---
    //
    // The `sun/misc/Unsafe` registration was RETIRED 2026-08-29.
    // `sun.misc.Unsafe.defineClass` is ABSENT from every supported image (JDK
    // 17.0.20.1+1, 21.0.12+8, 25.0.4+7 -- measured, not read off one image), so
    // nothing could dispatch to it, and it took 0 invocations across 118 corpus
    // vectors in both modes (`deprecated_verify.rs` now requires it ABSENT).
    //
    // The `jdk/internal/misc/Unsafe` twin was RETIRED in zt t14/df5. The
    // triple has `Code` on every supported image (the JDK body checks its
    // arguments and calls `defineClass0`, the one `Unsafe` define door,
    // `unsafe_natives::native_unsafe_define_class`), it is a retired shadow
    // (`retired_shadow::RETIRED_SHADOW_L5T_TRIPLES`: `--jdk-only` drops every
    // registration of it), and under `--compatible` `resolve_dispatch` picks the
    // bytecode. The t12 rows decided the one place it could still have served
    // (a by-name door running the LAST registration, which this re-run
    // registrar made it): `zt12df4_d5c` and `zt12df4_d5c_kill`
    // (`CRATONVM_ZGC_UNSAFE_DEFINE_CLASS_ONE_DOOR=0`, the twin's arm) were both
    // SAME with HotSpot, with the same five `door=unsafe` lines, so the twin
    // served nothing in either mode. Its switch went with it. The triple's
    // other registrations (`classloader.rs`, `unsafe_natives.rs`) are unchanged.

    // --- T8.4.2: off-heap memory operations ---
    // SECURITY FIX (V5): previously these routed to the *tracked* store
    // (`off_heap_store`, base 0x1_0000_0000) — a SECOND, disjoint off-heap
    // allocator from the *arena* store (base 0x10_0000_0000) that the raw
    // single-`long` get/put natives and freeMemory use. An address returned by
    // this `allocateMemory` was therefore NOT readable through the raw get/put
    // path `java.nio.Bits` relies on, and the two stores could alias each
    // other's freed ranges. Route allocate/reallocate/free/setMemory/copyMemory
    // through the SINGLE bounds-checked arena store (the one carrying the
    // per-thread use-after-free cache) so every off-heap address agrees on one
    // address space. `register_deprecated_internal_natives` is sometimes called
    // standalone AND, in the KC26 real-JDK boot path, re-invoked AFTER
    // `register_essential_natives` — so wiring the consolidated store here is
    // required to keep the live wiring single-store on every path, not just
    // when `register_unsafe_wp1_2` happens to run last.
    //
    // The legacy `native_tracked_*` / `tracked_*` / `off_heap_store` code below
    // is intentionally retained (now `#[allow(dead_code)]`) rather than deleted,
    // per the V5 directive to keep the unused store's code if removing it is
    // risky. Nothing is wired to it any longer.
    crate::unsafe_natives::register_consolidated_off_heap_store(r);
    r.set_category(__prev_cat);
}

#[allow(dead_code)]
fn native_tracked_allocate_memory(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe_this, size]
    let size = match args.get(1) {
        Some(Value::Long(s)) => *s,
        Some(Value::Int(s)) => *s as i64,
        _ => 0,
    };
    if size < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("allocateMemory: negative size {}", size),
        }
        .into());
    }
    let addr = tracked_allocate(size as usize);
    Ok(Some(Value::Long(addr as i64)))
}

#[allow(dead_code)]
fn native_tracked_free_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe_this, address]
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a as u64,
        Some(Value::Int(a)) => *a as u64,
        _ => return Ok(None),
    };
    if addr == 0 {
        // Freeing null is a no-op (matches native behavior)
        return Ok(None);
    }
    if !tracked_free(addr) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("freeMemory: invalid or already-freed address 0x{:x}", addr),
        }
        .into());
    }
    Ok(None)
}

#[allow(dead_code)]
fn native_tracked_realloc_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe_this, old_address, new_size]
    let old_addr = match args.get(1) {
        Some(Value::Long(a)) => *a as u64,
        Some(Value::Int(a)) => *a as u64,
        _ => 0,
    };
    let new_size = match args.get(2) {
        Some(Value::Long(s)) => *s,
        Some(Value::Int(s)) => *s as i64,
        _ => 0,
    };
    if new_size < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("reallocateMemory: negative size {}", new_size),
        }
        .into());
    }
    if old_addr == 0 {
        // realloc(NULL, size) == alloc(size)
        let addr = tracked_allocate(new_size as usize);
        return Ok(Some(Value::Long(addr as i64)));
    }
    match tracked_realloc(old_addr, new_size as usize) {
        Ok(new_addr) => Ok(Some(Value::Long(new_addr as i64))),
        Err(msg) => Err(RuntimeError::IllegalArgumentException {
            message: format!("reallocateMemory: {}", msg),
        }
        .into()),
    }
}

#[allow(dead_code)]
fn native_tracked_set_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe_this, object_or_null, offset, bytes_count, value]
    // For off-heap: object is null, offset is the address
    let addr = match args.get(2) {
        Some(Value::Long(a)) => *a as u64,
        Some(Value::Int(a)) => *a as u64,
        _ => 0,
    };
    let count = match args.get(3) {
        Some(Value::Long(c)) => *c as usize,
        Some(Value::Int(c)) => *c as usize,
        _ => 0,
    };
    let value = match args.get(4) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };

    // If object arg is non-null, this is on-heap; delegate to field-level ops
    if let Some(Value::Object(Some(_obj))) = args.get(1) {
        // On-heap setMemory not supported in tracked mode; silently succeed
        return Ok(None);
    }

    if addr == 0 {
        return Ok(None);
    }

    // Find the block that contains this address
    let store = off_heap_store().lock().unwrap_or_else(|e| e.into_inner());
    for (&block_addr, block) in store.iter() {
        if addr >= block_addr && (addr - block_addr) < block.len() as u64 {
            let offset = (addr - block_addr) as usize;
            drop(store);
            tracked_set_memory(block_addr, offset, count, value);
            return Ok(None);
        }
    }
    drop(store);

    // Try direct address match (addr IS the block address with offset 0)
    tracked_set_memory(addr, 0, count, value);
    Ok(None)
}

#[allow(dead_code)]
fn native_tracked_copy_memory(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe_this, src_obj, src_offset, dst_obj, dst_offset, bytes]
    let src_offset = match args.get(2) {
        Some(Value::Long(o)) => *o as u64,
        Some(Value::Int(o)) => *o as u64,
        _ => 0,
    };
    let dst_offset = match args.get(4) {
        Some(Value::Long(o)) => *o as u64,
        Some(Value::Int(o)) => *o as u64,
        _ => 0,
    };
    let count = match args.get(5) {
        Some(Value::Long(c)) => *c as usize,
        Some(Value::Int(c)) => *c as usize,
        _ => 0,
    };

    // If both objects are null, this is purely off-heap
    let src_null = matches!(args.get(1), Some(Value::Object(None)) | None);
    let dst_null = matches!(args.get(3), Some(Value::Object(None)) | None);

    if src_null && dst_null {
        // Find which blocks these offsets belong to
        let store = off_heap_store().lock().unwrap_or_else(|e| e.into_inner());
        let mut src_block_addr = src_offset;
        let mut src_inner_offset = 0usize;
        let mut dst_block_addr = dst_offset;
        let mut dst_inner_offset = 0usize;
        for (&block_addr, block) in store.iter() {
            if src_offset >= block_addr as u64
                && (src_offset - block_addr as u64) < block.len() as u64
            {
                src_block_addr = block_addr as u64;
                src_inner_offset = (src_offset - block_addr as u64) as usize;
            }
            if dst_offset >= block_addr as u64
                && (dst_offset - block_addr as u64) < block.len() as u64
            {
                dst_block_addr = block_addr as u64;
                dst_inner_offset = (dst_offset - block_addr as u64) as usize;
            }
        }
        drop(store);
        tracked_copy_memory(
            src_block_addr,
            src_inner_offset,
            dst_block_addr,
            dst_inner_offset,
            count,
        );
    }

    Ok(None)
}

// ===========================================================================
// T8.4.3 — sun.reflect.Reflection.getCallerClass
// ===========================================================================

/// `jdk.internal.reflect.Reflection.ensureNativeAccess(Class currentClass,
/// Class owner, String methodName, boolean jni)` — JEP 472's restricted-method
/// announce point, called by `Linker`, `MemorySegment.reinterpret`,
/// `SymbolLookup.libraryLookup` and `System.load*` before doing the restricted
/// thing.
///
/// Resolves the caller's module from the `currentClass` mirror and asks the
/// recorded `--enable-native-access` policy about it. Not granted ⇒ warn once
/// per module (the JDK 24/25 default `--illegal-native-access=warn`); the
/// actual denial happens at the operations themselves.
fn native_reflection_ensure_native_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // static (Class, Class, String, Z)V — args[0] is `currentClass`.
    let module = match args.first() {
        Some(Value::Object(Some(mirror))) => ctx
            .class_id_from_mirror(*mirror)
            .and_then(|cid| ctx.module_name_of_class(cid)),
        _ => None,
    };
    // `java.base` is implicitly granted native access and is never warned about.
    if module.as_deref() == Some("java.base") {
        return Ok(None);
    }
    if crate::panama::module_native_access_enabled(ctx, module.as_deref()) {
        return Ok(None);
    }

    // zt t18/df7: once per module per VM (`panama::NativeAccessState`; a
    // process `OnceLock` before).
    let key = module.unwrap_or_else(|| "ALL-UNNAMED".to_string());
    if !crate::panama::native_access(ctx).first_warning_for_module(&key) {
        return Ok(None);
    }

    let owner = match args.get(1) {
        Some(Value::Object(Some(mirror))) => ctx
            .class_id_from_mirror(*mirror)
            .and_then(|cid| ctx.class_name_of_id(cid))
            .map(|n| n.class_text(&*ctx))
            .unwrap_or_else(|| "<unknown>".to_string()),
        _ => "<unknown>".to_string(),
    };
    let method = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    tracing::warn!(
        target: "native_access",
        "A restricted method in {} has been called: {}.{}; module {} was not \
         granted --enable-native-access. Restricted methods will be blocked in \
         a future release.",
        owner,
        owner,
        method,
        key
    );
    Ok(None)
}

/// The tail asked for is too short to settle the caller walk; ask for more.
struct NeedsWiderTail;

/// Which entry of `frames` (OUTERMOST-first, the innermost entries of the whole
/// trace; `whole` says it IS the whole trace) `Reflection.getCallerClass()`
/// answers, as an index, or `None` for null; `Err` when the tail ends before the
/// walk does.
///
/// The rule, unchanged from the whole-trace capture it replaced: the top entry
/// is the `@CallerSensitive` method that called us (B), the answer is the entry
/// below it (A, `frames[len-2]`), skipping outward over java.util.stream /
/// Spliterator / ServiceLoader plumbing.
///
/// `ServiceLoader.load(Class)` is itself `@CallerSensitive` and calls this
/// native directly -- the "-2" is exactly right when `frames[n-1]` IS
/// `java/util/ServiceLoader`. But a `Stream.forEach(lambda)` call chain into
/// `ServiceLoader.load` (Hibernate's
/// `ClassLoaderServiceImpl.loadJavaServiceImplementors`, reached from JUnit's
/// `ClassBasedTestDescriptor.invokeTestInstancePostProcessors`'s
/// `list.forEach(postProcessor -> ...)`) is small, hot and gets
/// JIT-compiled/inlined as one unit -- MEASURED: `frames[n-1]` then comes back
/// as `java/util/stream/ForEachOps$ForEachOp$OfRef` (the forEach machinery),
/// not `ServiceLoader`, with every frame from the lambda down through
/// `ServiceLoader.load`/`checkCaller` missing entirely. Plain "-2" then lands
/// one frame INTO the same stream plumbing instead of on the real
/// (unnamed-module) caller, and `ServiceLoader.checkCaller` reports the caller
/// module as `java.base` -- which does not declare `uses` for a caller-supplied
/// service, and never would, because java.base is never the real caller here.
/// Skip outward through the plumbing to the first frame that is not
/// stream/Spliterator/ServiceLoader machinery instead of trusting one fixed
/// offset. See docs/feature-designs/native-dispatch-architecture.md
/// serviceloader-collapsed-inline-chain-caller-attribution-FIXED-20260922.md.
fn caller_frame_index(
    frames: &[cratonvm_native_api::StackTraceEntry],
    whole: bool,
) -> Result<Option<usize>, NeedsWiderTail> {
    match frames.len() {
        0 => return if whole { Ok(None) } else { Err(NeedsWiderTail) },
        // A single entry means "the whole trace is one frame"; a tail of one
        // says nothing about what lies below it.
        1 => {
            return if whole {
                Ok(Some(0))
            } else {
                Err(NeedsWiderTail)
            }
        }
        _ => {}
    }
    let mut idx = frames.len() - 2;
    loop {
        let name = frames[idx].class_name.as_ref();
        let is_plumbing = name.starts_with("java/util/stream/")
            || name == "java/util/ServiceLoader"
            || (name.starts_with("java/util/") && name.contains("Spliterator"));
        if !is_plumbing {
            return Ok(Some(idx));
        }
        if idx == 0 {
            // The whole trace ends here: answer this frame. A tail that ends
            // here may have more plumbing below it.
            return if whole {
                Ok(Some(0))
            } else {
                Err(NeedsWiderTail)
            };
        }
        idx -= 1;
    }
}

/// What `Reflection.getCallerClass()` reads off the frame it settled on.
struct CallerFrameShim {
    class_id: Option<cratonvm_types::ClassId>,
    class_name: String,
}

/// [`caller_frame_index`] over class ids: `ids` is INNERMOST-first (`ids[0]` is
/// the `@CallerSensitive` method that called us, `ids[1]` its caller), the
/// answer an index into `ids`. Same rule, same plumbing test, same
/// `NeedsWiderTail` contract: a tail that ends before the walk does is refused,
/// unless `whole`.
fn caller_id_index(
    ctx: &dyn NativeContext,
    ids: &[cratonvm_types::ClassId],
    whole: bool,
) -> Result<Option<usize>, NeedsWiderTail> {
    match ids.len() {
        0 => return if whole { Ok(None) } else { Err(NeedsWiderTail) },
        1 => {
            return if whole {
                Ok(Some(0))
            } else {
                Err(NeedsWiderTail)
            }
        }
        _ => {}
    }
    let mut idx = 1usize;
    loop {
        let is_plumbing = ctx.class_name_arc_of_id(ids[idx]).is_some_and(|name| {
            name.starts_with("java/util/stream/")
                || &*name == "java/util/ServiceLoader"
                || (name.starts_with("java/util/") && name.contains("Spliterator"))
        });
        if !is_plumbing {
            return Ok(Some(idx));
        }
        if idx + 1 == ids.len() {
            return if whole {
                Ok(Some(idx))
            } else {
                Err(NeedsWiderTail)
            };
        }
        idx += 1;
    }
}

fn register_reflection_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let refl = "sun/reflect/Reflection";

    // getCallerClass(int depth) -> Class — deprecated depth-based form
    r.register(
        refl,
        "getCallerClass",
        "(I)Ljava/lang/Class;",
        |ctx, args| {
            let depth = match args.get(0) {
                Some(Value::Int(d)) => *d,
                _ => 0,
            };

            if depth == 0 {
                // Return Reflection.class itself
                let cid = ctx
                    .ensure_class_initialized("sun/reflect/Reflection")
                    .unwrap_or(cratonvm_types::ClassId::new(0));
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }

            // Walk the stack to the given depth
            // Use capture_stack_trace with a dummy hash to get frames
            let frames = ctx.capture_stack_trace();
            if depth as usize <= frames.len() {
                let frame = &frames[(depth - 1) as usize];
                // Prefer the frame's own `class_id` (captured live from the
                // interpreter frame) over a name-based re-lookup. A name-keyed
                // lookup collapses to whichever class of that name loaded
                // FIRST/globally-registered, which is wrong whenever the actual
                // caller was loaded by a distinct ClassLoader from a same-named
                // class elsewhere on the classpath (e.g. a custom parentless
                // ClassLoader that `defineClass`-loads its own copy of a class
                // also present on the system classpath — see H2 `Upgrade.loadH2`'s
                // dynamic-driver-loading pattern, `DriverManager.deregisterDriver`'s
                // caller-classloader check). See `StackTraceEntry::class_id`'s doc
                // comment for the matching guidance.
                let cid = match frame.class_id {
                    Some(cid) => cid,
                    None => {
                        let class_name = frame.class_name.replace('.', "/");
                        ctx.ensure_class_initialized(&class_name)
                            .unwrap_or(cratonvm_types::ClassId::new(0))
                    }
                };
                let mirror = ctx.get_class_mirror(cid);
                Ok(Some(Value::Object(Some(mirror))))
            } else {
                // depth beyond stack — return null
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // getCallerClass() -> Class — JDK 8+ form (no-arg, skips framework frames)
    r.register(
        refl,
        "getCallerClass",
        "()Ljava/lang/Class;",
        |ctx, _args| {
            let frames = ctx.capture_stack_trace();
            // Skip frame 0 (Reflection itself) and frame 1 (immediate caller)
            // Return frame 2 (the actual caller)
            if frames.len() > 2 {
                let frame = &frames[2];
                // Prefer the frame's own `class_id` (captured live from the
                // interpreter frame) over a name-based re-lookup. A name-keyed
                // lookup collapses to whichever class of that name loaded
                // FIRST/globally-registered, which is wrong whenever the actual
                // caller was loaded by a distinct ClassLoader from a same-named
                // class elsewhere on the classpath (e.g. a custom parentless
                // ClassLoader that `defineClass`-loads its own copy of a class
                // also present on the system classpath — see H2 `Upgrade.loadH2`'s
                // dynamic-driver-loading pattern, `DriverManager.deregisterDriver`'s
                // caller-classloader check). See `StackTraceEntry::class_id`'s doc
                // comment for the matching guidance.
                let cid = match frame.class_id {
                    Some(cid) => cid,
                    None => {
                        let class_name = frame.class_name.replace('.', "/");
                        ctx.ensure_class_initialized(&class_name)
                            .unwrap_or(cratonvm_types::ClassId::new(0))
                    }
                };
                let mirror = ctx.get_class_mirror(cid);
                Ok(Some(Value::Object(Some(mirror))))
            } else {
                // Not enough frames — return null
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // Also register under jdk.internal.reflect for modern JDKs
    let refl2 = "jdk/internal/reflect/Reflection";
    r.register(
        refl2,
        "getCallerClass",
        "(I)Ljava/lang/Class;",
        |ctx, args| {
            // Depth-variant: depth=0 => Reflection itself; depth=1 =>
            // the immediate caller (the @CallerSensitive method);
            // depth=2 => its caller; and so on.  Frames are stored with
            // main at index 0 and innermost last; the native frame isn't
            // in the Vec.  So depth=N corresponds to frames[len-N].
            let depth = match args.get(0) {
                Some(Value::Int(d)) => *d,
                _ => 0,
            };
            if depth == 0 {
                let cid = ctx
                    .ensure_class_initialized("jdk/internal/reflect/Reflection")
                    .unwrap_or(cratonvm_types::ClassId::new(0));
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
            // The `depth` innermost entries are all this reads (SBR wave 6,
            // lane dispatch): `frames[len - depth]` is the first of them, and a
            // tail shorter than `depth` is the whole trace, which is too short.
            let want = usize::try_from(depth).unwrap_or(0);
            let (frames, _whole) = ctx.capture_stack_trace_tail(want);
            let target = if want <= frames.len() {
                frames.get(frames.len() - want)
            } else {
                None
            };
            if let Some(frame) = target {
                // Prefer the frame's own `class_id` (captured live from the
                // interpreter frame) over a name-based re-lookup. A name-keyed
                // lookup collapses to whichever class of that name loaded
                // FIRST/globally-registered, which is wrong whenever the actual
                // caller was loaded by a distinct ClassLoader from a same-named
                // class elsewhere on the classpath (e.g. a custom parentless
                // ClassLoader that `defineClass`-loads its own copy of a class
                // also present on the system classpath — see H2 `Upgrade.loadH2`'s
                // dynamic-driver-loading pattern, `DriverManager.deregisterDriver`'s
                // caller-classloader check). See `StackTraceEntry::class_id`'s doc
                // comment for the matching guidance.
                let cid = match frame.class_id {
                    Some(cid) => cid,
                    None => {
                        let class_name = frame.class_name.replace('.', "/");
                        ctx.ensure_class_initialized(&class_name)
                            .unwrap_or(cratonvm_types::ClassId::new(0))
                    }
                };
                let mirror = ctx.get_class_mirror(cid);
                Ok(Some(Value::Object(Some(mirror))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );

    r.register_with_kind(
        refl2,
        "getCallerClass",
        "()Ljava/lang/Class;",
        |ctx, _args| {
            // Reflection.getCallerClass() semantics:
            //   A -> B (@CallerSensitive) -> Reflection.getCallerClass() => return A
            //
            // capture_stack_trace returns frames in Vec order: frames[0]
            // is the bottom (main), frames[len-1] is the top (innermost
            // Java method).  The currently-executing native isn't a Java
            // frame, so the top is the @CallerSensitive method that
            // invoked us (B).  The caller we want to return is one frame
            // deeper toward the bottom: frames[len-2].
            //
            // For KC16 the typical chain is:
            //   SecureClassLoader.<clinit>                (frames[len-2]) <- return this
            //   ClassLoader.registerAsParallelCapable     (frames[len-1])
            //   [native Reflection.getCallerClass — not in Vec]
            //
            // HotSpot's rule over the walkers' capture (interpreter round i1
            // wave 51, lane L3; `VmConfig::reflection_caller_class_rule`,
            // `CRATONVM_REFLECTION_CALLER_RULE`): see
            // `stack_walker::reflection_get_caller_class_by_security_walk`.
            // Below is the older rule, kept behind the switch.
            if ctx.reflection_caller_class_rule() {
                return crate::stack_walker::reflection_get_caller_class_by_security_walk(ctx);
            }
            // SBR wave 6 (lane dispatch): only the innermost few entries are
            // read, so the capture is a bounded TAIL of the whole trace
            // (`capture_stack_trace_tail`), widened until the walk below
            // settles inside it or the tail is the whole trace. The answer is
            // the whole trace's answer: `CRATONVM_CALLER_TAIL=verify` checks it.
            // SBR round 2 (lane CALLER): two entries settle the walk whenever
            // the caller (`frames[len-2]`) is not stream plumbing, which is
            // almost always; a plumbing hit widens x4 as before. Building four
            // entries where two are read was measurable per `Field.get*`.
            //
            // SBR2 wave 8 (lane ctorplan): the walk reads class IDS only
            // (`frame_class_ids_tail`, the same frame set `capture_stack_trace_tail`
            // builds its entries from) and names a frame only to test it for
            // stream plumbing: no `StackTraceEntry`, method name or line number
            // is built per call. Every `Constructor.newInstance` the JDK's own
            // bytecode reaches through `Class.newInstance` and every
            // `StackFrameBuffer` slot asks this.
            let mut want = 3usize;
            let (ids, target) = loop {
                let (ids, whole) = ctx.frame_class_ids_tail(want);
                match caller_id_index(&*ctx, &ids, whole) {
                    Ok(idx) => break (ids, idx),
                    Err(NeedsWiderTail) => want = want.saturating_mul(4),
                }
            };
            let target = target.map(|i| CallerFrameShim {
                class_id: Some(ids[i]),
                class_name: String::new(),
            });
            if crate::nbflags().dbg_caller {
                eprintln!(
                    "[DBG_CALLER] jdk/internal/reflect/Reflection.getCallerClass trace ({} frames): {:?}",
                    ids.len(),
                    ids.iter()
                        .map(|c| ctx.class_name_arc_of_id(*c))
                        .collect::<Vec<_>>()
                );
            }
            if let Some(frame) = target {
                // Prefer the frame's own `class_id` (captured live from the
                // interpreter frame) over a name-based re-lookup. A name-keyed
                // lookup collapses to whichever class of that name loaded
                // FIRST/globally-registered, which is wrong whenever the actual
                // caller was loaded by a distinct ClassLoader from a same-named
                // class elsewhere on the classpath (e.g. a custom parentless
                // ClassLoader that `defineClass`-loads its own copy of a class
                // also present on the system classpath — see H2 `Upgrade.loadH2`'s
                // dynamic-driver-loading pattern, `DriverManager.deregisterDriver`'s
                // caller-classloader check). See `StackTraceEntry::class_id`'s doc
                // comment for the matching guidance.
                let cid = match frame.class_id {
                    Some(cid) => cid,
                    None => {
                        let class_name = frame.class_name.replace('.', "/");
                        ctx.ensure_class_initialized(&class_name)
                            .unwrap_or(cratonvm_types::ClassId::new(0))
                    }
                };
                let mirror = ctx.get_class_mirror(cid);
                Ok(Some(Value::Object(Some(mirror))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
        NativeKind::Bridge,
    );

    // Reflection.getClassAccessFlags(Class) — returns the raw ACC_ flags of
    // the class.  KC16 bootstrap calls this during URLClassPath.<clinit>; if
    // unregistered we throw UnsatisfiedLinkError and the classloader init
    // fails, cascading into the "Cannot invoke loadModule on null" NPE.
    //
    // HotSpot returns the unexpanded ACC_PUBLIC|FINAL|INTERFACE|ABSTRACT
    // bitmask.  This used to be a hardcoded PUBLIC (0x0001) for EVERY class,
    // which is not a conservative default but an active misstatement: an
    // interface was not reported as ACC_INTERFACE, an abstract class not as
    // ACC_ABSTRACT, and a package-private class was reported public — so every
    // JDK access/assignability check reading these flags silently saw the same
    // answer regardless of the class it asked about.  CratonVM DOES track the
    // class-file access flags (`NativeClassAccess::class_access_flags`); report
    // them.  The 0x0001 fallback is kept for the case where the argument is not
    // a resolvable class mirror, so no caller that works today starts failing.
    r.register_with_kind(
        refl2,
        "getClassAccessFlags",
        "(Ljava/lang/Class;)I",
        |ctx, args| {
            // HotSpot's `JVM_GetClassAccessFlags` answers a primitive (and
            // `void`) `ABSTRACT | FINAL | PUBLIC`; it read the PUBLIC fallback
            // below (interpreter round i4 wave 84, lane L3;
            // `L3W84ClassAccessFlags`).
            if let Some(Value::Object(Some(mirror))) = args.first() {
                if crate::lang_class::class_mirror_is_primitive(ctx, *mirror) {
                    return Ok(Some(Value::Int(0x0411)));
                }
            }
            let cid = match args.first() {
                Some(Value::Object(Some(mirror))) => ctx.class_id_from_mirror(*mirror),
                _ => None,
            };
            // A lambda proxy is minted outside the class store, so it has no
            // flags there and read back the PUBLIC fallback. HotSpot answers
            // the spun hidden class's `ACC_SUPER | ACC_FINAL | ACC_SYNTHETIC`
            // (0x1030, measured on 25.0.3): with PUBLIC the JDK's
            // `VerifyAccess.isClassAccessible` admitted the proxy to a public
            // `Lookup`, whose refusal then named the member ("class is not
            // public: P.run()void/invokeVirtual") where HotSpot names the class
            // ("symbolic reference class is not accessible: class P"),
            // `--jdk-only` (interpreter round i2 wave 62, lane L4;
            // `L4W61LambdaProxyHandleGaps` row `public-lookup`). Under
            // `CRATONVM_MH_LAMBDA_PROXY_MEMBERS` (`=0` restores the fallback).
            if crate::nbflags().mh_lambda_proxy_members
                && cid.is_some_and(|cid| {
                    crate::lang_class::lambda_proxy_class_name(&*ctx, cid).is_some()
                })
            {
                return Ok(Some(Value::Int(0x1030)));
            }
            // A class the class table names answers its own flags, 0 included
            // (a class file whose `access_flags` is 0 -- package-private, no
            // `ACC_SUPER`, as a bytecode generator may write it -- read PUBLIC
            // here, and the JDK's class-accessibility checks admitted it; i4
            // wave 84, lane L3). Only an unresolvable mirror keeps the PUBLIC
            // fallback.
            let flags = cid
                .filter(|cid| ctx.class_name_of_id(*cid).is_some())
                .map(|cid| i32::from(ctx.class_access_flags(cid)));
            Ok(Some(Value::Int(flags.unwrap_or(0x0001))))
        },
        NativeKind::Bridge,
    );
    // `Reflection.ensureNativeAccess` is the JEP 442/472 restricted-method
    // announce point (`--enable-native-access`). Now implemented, not a no-op.
    //
    // Two earlier justifications for the no-op were both wrong. The first
    // claimed CratonVM "grants native access to every module unconditionally";
    // it does not — `panama::NativeAccessPolicy` records the
    // `--enable-native-access` grant faithfully (`None` / `All` / per-module)
    // and defaults to `None`. The second claimed "the caller's MODULE is not
    // derivable from a mirror here"; it is —
    // `NativeContext::module_name_of_class` does exactly that, and is what
    // `module_native_access_enabled` wants.
    //
    // The announce point warns rather than throws, matching the JDK 24/25
    // default `--illegal-native-access=warn`. The hard denial stays at the
    // operations themselves, where CratonVM already enforces it:
    // `panama::require_native_access` on the downcall, raw-address
    // `MemorySegment` and `SymbolLookup.libraryLookup` paths, and
    // `security_manager::check_host_native_access_or_throw` on
    // `System.load`/`loadLibrary`, `Runtime.load*`,
    // `NativeLibraries.load` and `RawNativeLibraries.load0`. Throwing *here*
    // instead would deny under the default `NativeAccessPolicy::None`, i.e.
    // on every announce, which is not the JDK's default posture.
    r.register(
        refl2,
        "ensureNativeAccess",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;Z)V",
        native_reflection_ensure_native_access,
    );

    // ClassLoader.registerAsParallelCapable()Z — called from every
    // ClassLoader subclass's <clinit>.  The JDK implementation walks the
    // caller-class stack, resolves the caller's superclass through a
    // WeakHashMap-backed Set (ParallelLoaders.loaderTypes), and returns
    // true iff the superclass was pre-registered.  Our Class-mirror
    // identity path goes through WeakReference, which doesn't hold
    // objects strongly in our implementation — the Set appears empty and
    // register() returns false, causing BuiltinClassLoader.<clinit> to
    // throw InternalError("Unable to register as parallel capable").
    //
    // Pragmatic bypass: return true unconditionally.  Parallel capability
    // is a performance hint for the JDK's parallel class-loading lock
    // scheme; returning true is always safe for a single-threaded
    // bootstrap and any later classloader will silently accept it.
    //
    // NOT implementable from here, and MOSTLY DEAD.  This is one of three
    // registrations of the same triple that all answer 1
    // (`classloader.rs::cl_register_as_parallel_capable`,
    // `classloader_real.rs`).  `vm_init.rs` calls
    // `register_classloader_real_natives` AFTER
    // `register_deprecated_internal_natives` on the real-JDK path, and lib.rs
    // calls `classloader.rs` last on the synthetic path, so this copy only
    // wins in a registry built from `register_essential_natives_with_shims`
    // alone (unit tests).  The real fix is upstream of all three: make
    // `WeakHashMap`/`WeakReference` retain strongly-reachable `Class` keys so
    // the real `ParallelLoaders` bytecode works, then delete all three
    // registrations.  Until then the answer must stay `1` in every copy —
    // `false` aborts boot with the InternalError above.
    r.register(
        "java/lang/ClassLoader",
        "registerAsParallelCapable",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );

    // JBoss Modules ModuleLoader — MBean registration bypass.
    //
    // ModuleLoader.<init> runs a PrivilegedAction (the `$1` inner class)
    // to construct an ObjectName, wrap it in an MXBeanImpl, and register
    // the MBean.  The MXBeanImpl is backed by an org.jboss.modules.ref
    // WeakReference whose `Reaper` machinery tangles with our partial
    // concurrent subsystem, producing a bare IllegalStateException that
    // propagates up to DefaultBootModuleLoaderHolder.<clinit>, leaving
    // INSTANCE null and later failing Main.main with
    // "Cannot invoke loadModule on null".
    //
    // The JBoss bytecode at ModuleLoader.<init> wraps the whole action
    // (pc=111 checkcast ModuleLoaderMXBean) with a putfield that accepts
    // null.  So we can short-circuit by registering the two `$1.run()`
    // overloads to return null.  The ModuleLoader ends up with a null
    // mxBean field — harmless outside introspection — and <clinit>
    // completes cleanly.
    //
    // Classification: WORKAROUND for a VM bug elsewhere, not an unimplemented
    // stub. There is no "real" value to compute here — the only correct
    // implementation is to delete both registrations and let the JBoss
    // bytecode run. Exit criterion: `org.jboss.modules.ref`'s
    // WeakReference/`Reaper` machinery stops raising IllegalStateException
    // under CratonVM's concurrency subsystem.
    r.register(
        "org/jboss/modules/ModuleLoader$1",
        "run",
        "()Lorg/jboss/modules/management/ModuleLoaderMXBean;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        "org/jboss/modules/ModuleLoader$1",
        "run",
        "()Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // JBoss Modules LayeredModulePathFactory — layer expansion bypass.
    //
    // The Java implementation of resolveLayeredModulePath(File... roots)
    // walks each root, opens <root>/layers.conf, and for every configured
    // layer name X appends <root>/system/layers/X to the resulting
    // File[]. It validates each path with File.exists() and throws
    // IllegalStateException("No layers directory found at ...") when the
    // directory is missing.
    //
    // When invoked during DefaultBootModuleLoaderHolder.<clinit> under
    // CratonVM, one of those File.exists() checks returns false for a
    // directory that exists on disk (observed with Keycloak 16 where
    // modules/system/layers/keycloak is clearly present). The throw
    // unwinds into <clinit> of both DefaultBootModuleLoaderHolder and —
    // via WARN-level swallowing in vm_util — org/jboss/modules/Module,
    // leaving BOOT_MODULE_LOADER unset and blocking all subsequent
    // module lookups.
    //
    // Native override: reimplement the expansion directly against the
    // host filesystem using std::fs, which matches JBoss's intent
    // without tripping on any File.exists() edge case. For each input
    // root File we:
    //   1. always include the root itself;
    //   2. read <root>/layers.conf (if present) to determine the
    //      configured layer names; default to "base" when absent;
    //   3. append <root>/system/layers/<layer> for each layer that
    //      exists on disk, then overlays, then the mirror add-ons;
    //   4. silently skip anything that doesn't resolve — matching the
    //      "not configured" fallback in the original code rather than
    //      throwing.
    //
    // Functional cost: we don't honour overlays/.conf/.overlays refs or
    // add-on trees verbatim — those are rarely used in server bootstrap
    // paths. The Keycloak 16 happy path (single "keycloak" layer under
    // a standard modules/ directory) is fully supported.
    r.register(
        "org/jboss/modules/LayeredModulePathFactory",
        "resolveLayeredModulePath",
        "([Ljava/io/File;)[Ljava/io/File;",
        |ctx, args| {
            let roots_arr = match args.first() {
                Some(Value::Object(Some(r))) => *r,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("resolveLayeredModulePath: null roots".to_string()),
                    }
                    .into());
                }
            };
            // gc-common w19-g: every object this native holds across a Java
            // call or an allocation lives in this one scope. The input array
            // was read raw on every iteration after the previous root's
            // `File.getPath()` (Java), so a collection there made the later
            // roots come from the vacated copy.
            let mut scope = NativeHandleScope::new(ctx);
            let roots_h = scope.root(roots_arr);
            let n = scope.array_length(roots_arr);

            // Collect each input root's path via File.getPath()
            let mut out_paths: Vec<String> = Vec::new();
            for i in 0..n {
                let roots_now = scope.get(&roots_h);
                let elem = scope.get_array_element(roots_now, i);
                let root_ref = match elem {
                    Value::Object(Some(r)) => r,
                    _ => continue,
                };
                // Invoke File.getPath() to get the root's path string.
                let path_val = scope.invoke(
                    "java/io/File",
                    "getPath",
                    "()Ljava/lang/String;",
                    &[Value::Object(Some(root_ref))],
                )?;
                let root_path = match path_val {
                    Some(Value::Object(Some(s))) => scope.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                if root_path.is_empty() {
                    continue;
                }

                out_paths.push(root_path.clone());

                // Read layers.conf from the root to determine which
                // layers to expand.  Absent / unreadable → treat as
                // "not configured" and skip silently (matches the
                // isConfigured()==false branch of the JBoss code).
                let layers_conf = std::path::PathBuf::from(&root_path).join("layers.conf");
                let mut layer_names: Vec<String> = match std::fs::read_to_string(&layers_conf) {
                    Ok(contents) => {
                        let mut names = Vec::new();
                        for line in contents.lines() {
                            let trimmed = line.trim();
                            if let Some(rest) = trimmed.strip_prefix("layers=") {
                                for part in rest.split(',') {
                                    let p = part.trim();
                                    if !p.is_empty() {
                                        names.push(p.to_string());
                                    }
                                }
                            }
                        }
                        names
                    }
                    Err(_) => continue,
                };
                // JBoss LayersConfig always appends the implicit "base"
                // layer to the end of the user-configured list (see
                // LayeredModulePathFactory$LayersConfig pc=167-174:
                // `layers.add("base")` after the layers= entries are
                // parsed).  Without this, Keycloak's `layers=keycloak`
                // omits the `base` layer tree that actually contains
                // most `org/jboss/**` modules, producing
                // ModuleNotFoundException at runtime.  Avoid a duplicate
                // if the user listed "base" explicitly.
                if !layer_names.iter().any(|n| n == "base") {
                    layer_names.push("base".to_string());
                }

                let layers_root = std::path::PathBuf::from(&root_path)
                    .join("system")
                    .join("layers");
                for layer in &layer_names {
                    let layer_dir = layers_root.join(layer);
                    if layer_dir.is_dir() {
                        out_paths.push(layer_dir.to_string_lossy().into_owned());
                    }
                }

                // Include add-on entries if the tree is present.
                let addons_root = std::path::PathBuf::from(&root_path)
                    .join("system")
                    .join("add-ons");
                if addons_root.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&addons_root) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if p.is_dir() {
                                out_paths.push(p.to_string_lossy().into_owned());
                            }
                        }
                    }
                }
            }

            // Build File[] result.
            //
            // gc-common w19-g: the array, each path `String` and each `File`
            // are rooted across the allocations and the `File.<init>` call
            // (Java) that follow them, and re-read before use. All three were
            // held raw, so a collection inside the loop stored the `File`s
            // into the array's vacated copy and returned that copy.
            let file_cid = scope.ensure_class_initialized("java/io/File")?;
            let out_arr_new = scope.new_ref_array(file_cid, out_paths.len());
            let out_arr_h = scope.root(out_arr_new);
            for (i, p) in out_paths.iter().enumerate() {
                let path_str_new = scope.create_string(p);
                let path_str_h = scope.root(path_str_new);
                let file_new = match scope.new_object("java/io/File")? {
                    Some(Value::Object(Some(f))) => f,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "failed to allocate java/io/File".to_string(),
                        }
                        .into());
                    }
                };
                let file_h = scope.root(file_new);
                let path_str = scope.get(&path_str_h);
                scope.invoke(
                    "java/io/File",
                    "<init>",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(file_new)), Value::Object(Some(path_str))],
                )?;
                let file_obj = scope.get(&file_h);
                let out_arr = scope.get(&out_arr_h);
                scope.set_array_element(out_arr, i, Value::Object(Some(file_obj)));
            }
            let out_arr = scope.get(&out_arr_h);
            Ok(Some(Value::Object(Some(out_arr))))
        },
    );

    // JBoss PathUtils.basicModuleNameToPath — bypass Normalizer dependency.
    //
    // The real implementation normalizes the module name via
    // java.text.Normalizer (NFKC form), which routes through ICU's
    // Normalizer2/NormalizerBase.  Our ICU path has resource-loading gaps
    // that cause an AIOOBE in ICUBinary.readHeader, leaving the normalized
    // string as null and causing PathUtils to return null — every
    // LocalModuleFinder.findModule call then produces ModuleNotFoundException
    // for every module, blocking the entire JBoss module graph.
    //
    // The method is a pure path transformation: replace `.` with `/` after
    // an optional slot suffix `module:slot`.  Module names are ASCII-only
    // in every JBoss module.xml we've seen, so NFKC normalization is a
    // no-op for them.  Skip it entirely.
    r.register(
        "org/jboss/modules/PathUtils",
        "basicModuleNameToPath",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            if name.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            // Split optional `:slot` suffix.
            let (module_part, slot_part) = match name.rfind(':') {
                Some(i) => (&name[..i], Some(&name[i + 1..])),
                None => (name.as_str(), None),
            };
            let mut out = module_part.replace('.', "/");
            if let Some(slot) = slot_part {
                out.push('/');
                out.push_str(slot);
            } else {
                out.push('/');
                out.push_str("main");
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&out)))))
        },
    );

    // Throwable.toString() was previously re-registered HERE with a
    // raw-field-scanning implementation (format `ClassName: message`,
    // reading field slots 0..5 directly for any non-empty String) meant to
    // fix JBoss Modules printing `org.jboss.modules.ModuleNotFoundException@0`
    // instead of the module name (dispatch was resolving to
    // `Object.toString()`, losing the message).
    //
    // Removed 2026-07-22: this was a duplicate/last-write-wins registration
    // that silently clobbered the correct `java/lang/Throwable.toString()`
    // native (`native_throwable_to_string` in lang_misc.rs, registered
    // earlier by `register_throwable_subclass_natives` within this same
    // `register_essential_natives` call chain) — which already fixes the
    // JBoss Modules symptom AND does so correctly, via a real virtual
    // dispatch to `getLocalizedMessage()`/`getMessage()` (honoring any
    // subclass override), rather than a raw, override-blind field scan.
    // The raw-field version broke every Throwable subclass whose
    // getMessage()/getLocalizedMessage() override computes the message
    // dynamically rather than storing it in one of the receiver's own
    // first 6 field slots — e.g. `org.h2.jdbc.JdbcSQLSyntaxErrorException`
    // (H2's `TestLinkedTable.testHiddenSQL`), whose overridden
    // `getMessage()` returns a lazily-rebuilt `message` field appended with
    // the SQL statement text: `super.toString()` (an invokespecial to
    // `Throwable.toString()`) silently reverted to the ORIGINAL short
    // constructor message, dropping the SQL-statement suffix (and any
    // password the test asserts is present in it). See
    // bug-h2-suite-residual-fail-triage-FIXED.md.

    // JBoss Modules JDKModuleFinder.findModule — bypass.
    //
    // JDKModuleFinder synthesizes ModuleSpec objects for the JDK's own
    // named modules (java.se, java.compiler, etc.) by iterating
    // JavaSeDeps.list and assembling DependencySpec entries.  The
    // implementation calls ModuleSpec$Builder.build() which touches the
    // ModuleFinder/ModuleLoader graph, and we've observed a null receiver
    // NPE ("Cannot invoke findModule on null") propagate from inside the
    // build chain — likely because a ConcurrentHashMap lookup for the
    // FutureSpec returns null under our partial concurrent-subsystem
    // path and the bytecode assumes non-null.
    //
    // Returning null signals "this finder does not know about the
    // requested module".  JBoss Modules falls back to LocalModuleFinder
    // for everything else, which uses the on-disk module.xml tree — the
    // only path that actually resolves Keycloak's modules.  The JDK's
    // own modules are already loaded by our real-JDK bootstrap and are
    // accessible to bytecode via the regular class loader hierarchy.
    //
    // Classification: this is a SPEC-VALID answer, not a placeholder — the
    // `ModuleFinder` contract explicitly allows a finder to return null for a
    // module it does not provide, and the fallback finder is the one that
    // matters. Exit criterion is nonetheless the NPE named above (a
    // ConcurrentHashMap lookup returning null inside `ModuleSpec$Builder`),
    // after which both registrations can simply be deleted.
    r.register(
        "org/jboss/modules/JDKModuleFinder",
        "findModule",
        "(Ljava/lang/String;Lorg/jboss/modules/ModuleLoader;)Lorg/jboss/modules/ModuleSpec;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// T8.4.4 — sun.misc.Signal / jdk.internal.misc.Signal
// ===========================================================================

fn register_signal_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_signal_class(r, "sun/misc/Signal");
    register_signal_class(r, "jdk/internal/misc/Signal");
    r.set_category(__prev_cat);
}

/// The two fields `jdk.internal.misc.Signal` / `sun.misc.Signal` carry.
#[derive(Clone, Copy)]
enum SignalField {
    Name,
    Number,
}

impl SignalField {
    fn name(self) -> &'static str {
        match self {
            SignalField::Name => "name",
            SignalField::Number => "number",
        }
    }

    /// The slot to use when the name does not resolve. This is the layout a
    /// synthetic `Signal` stub has -- and, historically, the layout this
    /// registrar assumed for the REAL class too, which was the defect: `javap
    /// -p jdk.internal.misc.Signal` declares `private int number;` before
    /// `private java.lang.String name;`, so the real slots are the other way
    /// round from these.
    fn fallback_slot(self) -> usize {
        match self {
            SignalField::Name => 0,
            SignalField::Number => 1,
        }
    }
}

/// Write a `Signal` field by NAME, falling back to [`SignalField::fallback_slot`]
/// when the receiver resolves no such name.
///
/// The read-back is what makes the fallback safe rather than a guess: it tells
/// "the receiver has this field" apart from "the name resolved to nothing"
/// without this code having to know which kind of receiver it is holding. Same
/// shape as `lang_misc::write_throwable_field`, which exists for the same
/// reason and against the same two receiver kinds.
fn signal_field_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: SignalField,
    value: Value,
) {
    ctx.set_field_by_name(this, field.name(), value);
    if ctx.get_field_by_name(this, field.name()) != value {
        let slot = field.fallback_slot();
        if slot < ctx.object_num_fields(this) {
            ctx.set_field(this, slot, value);
        }
    }
}

/// [`signal_field_set`]'s reader. A resolved name wins; an unresolved one falls
/// back to the same slot the writer used.
fn signal_field_get(ctx: &mut dyn NativeContext, this: ObjectRef, field: SignalField) -> Value {
    let by_name = ctx.get_field_by_name(this, field.name());
    let unresolved = match field {
        // An unset or unresolvable reference slot reads back as `Object(None)`
        // or as the raw `Int(0)` of an untyped slot; a real signal always has a
        // name, so either means the name did not resolve.
        SignalField::Name => matches!(by_name, Value::Object(None) | Value::Int(0)),
        // Signal numbers are positive, so a zero means the same thing.
        SignalField::Number => matches!(by_name, Value::Int(0) | Value::Object(None)),
    };
    if !unresolved {
        return by_name;
    }
    let slot = field.fallback_slot();
    if slot < ctx.object_num_fields(this) {
        return ctx.get_field(this, slot);
    }
    by_name
}

fn register_signal_class(r: &mut NativeMethodRegistry, sig_class: &str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Signal.<init>(String)V — create Signal from name
    // Signal object: field 0 = name (String), field 1 = number (Int)
    r.register(sig_class, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name_obj = obj_arg(args, 1)?;
        let name = ctx.read_string(name_obj).unwrap_or_default();

        let number = signal_name_to_number(&name).unwrap_or(-1);
        if number < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Unknown signal: {}", name),
            }
            .into());
        }

        // BY NAME, NOT BY SLOT. The comment above this registrar said "field 0
        // = name (String), field 1 = number (Int)" and the real class declares
        // them the other way round:
        //
        //   javap -p jdk.internal.misc.Signal
        //     private int number;
        //     private java.lang.String name;
        //
        // so the constructor wrote a String reference into an `int` slot and an
        // `int` into a reference slot. `getName()` came back null, `getNumber()`
        // came back 0, and `toString()` -- real bytecode reading `this.name` --
        // printed `SIGnull` for every signal. `equals` then threw an NPE from
        // inside the JDK. MEASURED by `apps/probes/JdkInternalSweep.java`: 12
        // rows, all of them this one line.
        signal_field_set(ctx, this, SignalField::Name, Value::Object(Some(name_obj)));
        signal_field_set(ctx, this, SignalField::Number, Value::Int(number));
        Ok(None)
    });

    // getNumber()I — return signal number
    r.register(sig_class, "getNumber", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(signal_field_get(ctx, this, SignalField::Number)))
    });

    // getName()Ljava/lang/String; — return signal name
    r.register(sig_class, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(signal_field_get(ctx, this, SignalField::Name)))
    });

    // handle(Signal, SignalHandler) -> SignalHandler — register a handler
    let handler_sig = if sig_class.starts_with("sun") {
        "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;"
    } else {
        "(Ljdk/internal/misc/Signal;Ljdk/internal/misc/SignalHandler;)Ljdk/internal/misc/SignalHandler;"
    };
    r.register(sig_class, "handle", handler_sig, |ctx, args| {
        let signal_obj = obj_arg(args, 0)?;
        let handler = match args.get(1) {
            Some(Value::Object(Some(h))) => Some(*h),
            _ => None,
        };

        // By NAME, like `<init>` writes it (gc-common w10-a). Slot 1 is the
        // synthetic layout's `number`; on the real class it is `name`, so
        // every `handle` on a real `Signal` threw "has no signal number".
        let sig_num = match signal_field_get(ctx, signal_obj, SignalField::Number) {
            Value::Int(n) if n > 0 => n,
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Signal object has no signal number".to_string(),
                }
                .into());
            }
        };

        // Keep the new handler alive and remapped: a global root of this VM
        // (gc-common w29-d), taken on the argument's address; nothing
        // allocates in between.
        let entry = handler.map(|h| (ctx.add_global_root(h), h));
        // Swap old handler with new one, in this VM's table only.
        let old = SIGNAL_HANDLERS
            .with(ctx.vm_identity(), |store| store.insert(sig_num, entry))
            .flatten();
        // Resolve the replaced handler, then release its root: it is returned
        // to Java at once, with no allocation in between.
        let old_handler = old.map(|row| {
            let current = resolve_signal_handler(&*ctx, row);
            if row.0 != 0 {
                ctx.remove_global_root(row.0);
            }
            current
        });
        Ok(Some(Value::Object(old_handler)))
    });

    // raise(Signal)V — raise a signal (invoke registered handler)
    let raise_sig = if sig_class.starts_with("sun") {
        "(Lsun/misc/Signal;)V"
    } else {
        "(Ljdk/internal/misc/Signal;)V"
    };
    r.register(sig_class, "raise", raise_sig, |ctx, args| {
        let signal_obj = obj_arg(args, 0)?;
        let sig_num = match signal_field_get(ctx, signal_obj, SignalField::Number) {
            Value::Int(n) if n > 0 => n,
            _ => return Ok(None),
        };

        let handler = SIGNAL_HANDLERS
            .peek(ctx.vm_identity(), |store| store.get(&sig_num).copied())
            .flatten()
            .flatten()
            .map(|row| resolve_signal_handler(&*ctx, row));

        if let Some(handler_obj) = handler {
            // Invoke handler.handle(Signal) with the descriptor of the
            // signal's own package.
            let handler_desc = signal_handler_descriptor(ctx, signal_obj);
            let _ = ctx.invoke_virtual(
                handler_obj,
                "handle",
                handler_desc,
                &[Value::Object(Some(signal_obj))],
            );
        }

        Ok(None)
    });

    // Signal.number(String) -> int — static helper used internally
    r.register(sig_class, "number", "(Ljava/lang/String;)I", |ctx, args| {
        let name_obj = match args.get(0) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let name = ctx.read_string(name_obj).unwrap_or_default();
        let num = signal_name_to_number(&name).unwrap_or(-1);
        Ok(Some(Value::Int(num)))
    });
    r.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Helper: look up a native and call it through the registry.
    fn call_native(
        registry: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        class: &str,
        method: &str,
        desc: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let cb = registry
            .find(class, method, desc)
            .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
        cb(ctx, args)
    }

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_deprecated_internal_natives(&mut r);
        r
    }

    // --- T8.3.1: Beans.instantiate ---

    #[test]
    fn test_beans_instantiate_success() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // Create a class name string
        let name = ctx.create_string("com.example.MyBean");
        // ClassLoader is null
        let result = call_native(
            &reg,
            &mut ctx,
            "java/beans/Beans",
            "instantiate",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;",
            &[Value::Object(None), Value::Object(Some(name))],
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(matches!(val, Some(Value::Object(Some(_)))));
    }

    #[test]
    fn test_beans_instantiate_null_name() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg,
            &mut ctx,
            "java/beans/Beans",
            "instantiate",
            "(Ljava/lang/ClassLoader;Ljava/lang/String;)Ljava/lang/Object;",
            &[Value::Object(None), Value::Object(None)],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_beans_is_design_time() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        let result = call_native(
            &reg,
            &mut ctx,
            "java/beans/Beans",
            "isDesignTime",
            "()Z",
            &[],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    // --- T8.3.2: RemoteRef.getRefClass ---

    #[test]
    fn test_remote_ref_get_ref_class_empty() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg,
            &mut ctx,
            "java/rmi/server/RemoteRef",
            "getRefClass",
            "(Ljava/io/ObjectOutput;)Ljava/lang/String;",
            &[Value::Object(None)],
        );
        let val = result.unwrap().unwrap();
        if let Value::Object(Some(obj)) = val {
            let s = ctx.read_string(obj).unwrap();
            assert_eq!(s, "");
        } else {
            panic!("expected string object");
        }
    }

    // --- T8.3.3: Activation classes load ---

    #[test]
    fn test_activatable_init_no_error() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg,
            &mut ctx,
            "java/rmi/activation/Activatable",
            "<init>",
            "()V",
            &[],
        );
        assert!(result.is_ok());
    }

    /// `ActivationGroup.getSystem()` used to answer a constant null, which the
    /// caller could only discover by NPE-ing on it several frames later. It now
    /// throws like its `register`/`exportObject` siblings — `java.rmi.activation`
    /// was removed in JDK 17 and there is no activation system to return.
    #[test]
    fn test_activation_group_get_system_throws() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg,
            &mut ctx,
            "java/rmi/activation/ActivationGroup",
            "getSystem",
            "()Ljava/rmi/activation/ActivationSystem;",
            &[],
        );
        assert!(result.is_err(), "getSystem must not answer a silent null");
    }

    #[test]
    fn test_activatable_register_throws() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg,
            &mut ctx,
            "java/rmi/activation/Activatable",
            "register",
            "(Ljava/rmi/activation/ActivationDesc;)Ljava/rmi/activation/ActivationID;",
            &[Value::Object(None)],
        );
        assert!(result.is_err());
    }

    // --- T8.4.1: Unsafe.defineClass ---
    // The `jdk.internal.misc` twin's CAFEBABE tests went with the twin (zt
    // t14/df5); the one `Unsafe` door (`unsafe_natives::native_unsafe_define_class`)
    // carries its own tests.

    // --- T8.4.2: Unsafe memory lifecycle ---

    #[test]
    fn test_unsafe_memory_allocate_free() {
        let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // Allocate
        let result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "allocateMemory",
            "(J)J",
            &[Value::Object(None), Value::Long(1024)],
        );
        let addr = match result.unwrap() {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long, got {:?}", other),
        };
        assert!(addr > 0);

        // Free
        let free_result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "freeMemory",
            "(J)V",
            &[Value::Object(None), Value::Long(addr)],
        );
        assert!(free_result.is_ok());
    }

    // SECURITY FIX (V5): the off-heap natives now resolve to the single
    // bounds-checked *arena* store (base 0x10_0000_0000), not the old dual
    // "tracked" store this test used to exercise. The arena's free is
    // idempotent — use-after-free is caught by the bounds-checked accessors
    // (here `setMemory`), not by making the second `freeMemory` throw. So we
    // assert the LIVE invariant: after a free, the address is no longer in any
    // live arena, and `setMemory` on it is rejected. A live address still
    // accepts `setMemory`, proving the rejection is specific to the freed
    // range (not a blanket failure). This keeps double-free / UAF detection
    // under real test coverage against the consolidated store.
    #[test]
    fn test_unsafe_memory_double_free() {
        let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
        let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // Allocate
        let result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "allocateMemory",
            "(J)J",
            &[Value::Object(None), Value::Long(256)],
        );
        let addr = match result.unwrap() {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long, got {:?}", other),
        };

        // While live, an off-heap setMemory into the arena succeeds.
        let set_live = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "setMemory",
            "(Ljava/lang/Object;JJB)V",
            &[
                Value::Object(None), // off-heap target
                Value::Object(None), // null base object => off-heap
                Value::Long(addr),
                Value::Long(16),
                Value::Int(0xAB),
            ],
        );
        assert!(set_live.is_ok(), "setMemory on a live arena must succeed");

        // First free succeeds.
        let free1 = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "freeMemory",
            "(J)V",
            &[Value::Object(None), Value::Long(addr)],
        );
        assert!(free1.is_ok());

        // Second free is idempotent in the consolidated arena store (the
        // address is simply gone) — it must not panic and is a no-op.
        let free2 = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "freeMemory",
            "(J)V",
            &[Value::Object(None), Value::Long(addr)],
        );
        assert!(free2.is_ok());

        // Use-after-free is the real invariant: the freed address is no longer
        // in any live arena, so a bounds-checked off-heap write is rejected.
        let set_freed = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "setMemory",
            "(Ljava/lang/Object;JJB)V",
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(addr),
                Value::Long(16),
                Value::Int(0xCD),
            ],
        );
        assert!(
            set_freed.is_err(),
            "writing to a freed off-heap address must be rejected (UAF detection)"
        );
    }

    // SECURITY FIX (V5): reallocateMemory now routes to the arena store. The
    // arena resizes the block IN PLACE and keeps the same base address (a valid
    // realloc outcome — the JDK contract only promises the returned pointer is
    // usable, not that it differs). So the old `assert_ne!(new_addr, addr)`
    // (a tracked-store artifact that always handed out a fresh address) no
    // longer holds. We instead assert the meaningful realloc semantics against
    // the live store: the reallocated region is valid and writable up to the
    // NEW size, and realloc(NULL, size) behaves like a fresh allocation.
    #[test]
    fn test_unsafe_memory_realloc() {
        let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // Allocate 128 bytes.
        let result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "allocateMemory",
            "(J)J",
            &[Value::Object(None), Value::Long(128)],
        );
        let addr = match result.unwrap() {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long, got {:?}", other),
        };
        assert!(addr > 0);

        // Realloc to 512.
        let realloc_result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "reallocateMemory",
            "(JJ)J",
            &[Value::Object(None), Value::Long(addr), Value::Long(512)],
        );
        let new_addr = match realloc_result.unwrap() {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long, got {:?}", other),
        };
        assert!(new_addr > 0);

        // The grown region must be valid up to the NEW size: a write at the
        // far end of the 512-byte block must succeed (it would have been out
        // of bounds for the original 128-byte block).
        let set_grown = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "setMemory",
            "(Ljava/lang/Object;JJB)V",
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Long(new_addr + 256),
                Value::Long(128),
                Value::Int(0x7E),
            ],
        );
        assert!(
            set_grown.is_ok(),
            "the reallocated region must be writable up to the new size"
        );

        // realloc(NULL, size) == alloc(size): returns a fresh, distinct,
        // writable arena.
        let fresh = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "reallocateMemory",
            "(JJ)J",
            &[Value::Object(None), Value::Long(0), Value::Long(64)],
        );
        let fresh_addr = match fresh.unwrap() {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long, got {:?}", other),
        };
        assert!(fresh_addr > 0);
        assert_ne!(
            fresh_addr, new_addr,
            "realloc(NULL) must hand out a distinct arena"
        );

        // Free the reallocated address.
        let free_result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "freeMemory",
            "(J)V",
            &[Value::Object(None), Value::Long(new_addr)],
        );
        assert!(free_result.is_ok());
    }

    #[test]
    fn test_unsafe_memory_negative_size() {
        let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Unsafe",
            "allocateMemory",
            "(J)J",
            &[Value::Object(None), Value::Long(-1)],
        );
        assert!(result.is_err());
    }

    // --- T8.4.3: Reflection.getCallerClass ---

    #[test]
    fn test_reflection_get_caller_class_depth_zero() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // depth=0 should return Reflection.class itself
        let result = call_native(
            &reg,
            &mut ctx,
            "sun/reflect/Reflection",
            "getCallerClass",
            "(I)Ljava/lang/Class;",
            &[Value::Int(0)],
        );
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Some(Value::Object(Some(_)))));
    }

    #[test]
    fn test_reflection_get_caller_class_no_arg() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // No-arg form: in test context, stack is empty so returns null
        let result = call_native(
            &reg,
            &mut ctx,
            "sun/reflect/Reflection",
            "getCallerClass",
            "()Ljava/lang/Class;",
            &[],
        );
        assert!(result.is_ok());
        // Mock returns empty stack so null is expected
        assert_eq!(result.unwrap(), Some(Value::Object(None)));
    }

    // --- T8.4.4: Signal ---

    #[test]
    fn test_signal_name_to_number_mapping() {
        assert_eq!(signal_name_to_number("INT"), Some(2));
        assert_eq!(signal_name_to_number("TERM"), Some(15));
        assert_eq!(signal_name_to_number("HUP"), Some(1));
        assert_eq!(signal_name_to_number("KILL"), Some(9));
        assert_eq!(signal_name_to_number("SIGINT"), Some(2));
        assert_eq!(signal_name_to_number("BOGUS"), None);
    }

    #[test]
    fn test_signal_number_to_name() {
        assert_eq!(signal_number_to_name(2), "INT");
        assert_eq!(signal_number_to_name(15), "TERM");
        assert_eq!(signal_number_to_name(999), "UNKNOWN");
    }

    #[test]
    fn test_signal_init_and_get_number() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let cid = ctx.ensure_class_initialized("sun/misc/Signal").unwrap();
        let signal_obj = ctx.alloc_object(cid, 4);
        let name_str = ctx.create_string("INT");

        let result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Signal",
            "<init>",
            "(Ljava/lang/String;)V",
            &[
                Value::Object(Some(signal_obj)),
                Value::Object(Some(name_str)),
            ],
        );
        assert!(result.is_ok());

        // getNumber should return 2 (SIGINT)
        let num_result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Signal",
            "getNumber",
            "()I",
            &[Value::Object(Some(signal_obj))],
        );
        assert_eq!(num_result.unwrap(), Some(Value::Int(2)));
    }

    #[test]
    fn test_signal_handler_registration() {
        // A private VM id: the handler table is per VM (gc-common w10-a), so
        // this test owns its rows and never clears another test's.
        const VM: usize = 0xA10_5101;
        forget_vm_signal_handlers(VM);

        let reg = make_registry();
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);

        // Create a signal object for SIGINT
        let cid = ctx.ensure_class_initialized("sun/misc/Signal").unwrap();
        let signal_obj = ctx.alloc_object(cid, 4);
        let name_str = ctx.create_string("INT");
        call_native(
            &reg,
            &mut ctx,
            "sun/misc/Signal",
            "<init>",
            "(Ljava/lang/String;)V",
            &[
                Value::Object(Some(signal_obj)),
                Value::Object(Some(name_str)),
            ],
        )
        .unwrap();

        // Create a mock handler object
        let handler_cid = ctx
            .ensure_class_initialized("sun/misc/SignalHandler")
            .unwrap();
        let handler_obj = ctx.alloc_object(handler_cid, 2);

        // Register handler — first registration returns null (no previous handler)
        let handle_result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Signal",
            "handle",
            "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;",
            &[
                Value::Object(Some(signal_obj)),
                Value::Object(Some(handler_obj)),
            ],
        );
        assert!(handle_result.is_ok());
        // First registration — no previous handler, so null
        assert_eq!(handle_result.unwrap(), Some(Value::Object(None)));

        // Register another handler — should return previous handler
        let handler2 = ctx.alloc_object(handler_cid, 2);
        let handle_result2 = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Signal",
            "handle",
            "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;",
            &[
                Value::Object(Some(signal_obj)),
                Value::Object(Some(handler2)),
            ],
        );
        assert!(handle_result2.is_ok());
        // Should return the first handler
        assert!(matches!(
            handle_result2.unwrap(),
            Some(Value::Object(Some(_)))
        ));
    }

    #[test]
    fn test_signal_unknown_name_error() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let cid = ctx.ensure_class_initialized("sun/misc/Signal").unwrap();
        let signal_obj = ctx.alloc_object(cid, 4);
        let name_str = ctx.create_string("BOGUS");

        let result = call_native(
            &reg,
            &mut ctx,
            "sun/misc/Signal",
            "<init>",
            "(Ljava/lang/String;)V",
            &[
                Value::Object(Some(signal_obj)),
                Value::Object(Some(name_str)),
            ],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_jdk_internal_signal_same_behavior() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let cid = ctx
            .ensure_class_initialized("jdk/internal/misc/Signal")
            .unwrap();
        let signal_obj = ctx.alloc_object(cid, 4);
        let name_str = ctx.create_string("TERM");

        let result = call_native(
            &reg,
            &mut ctx,
            "jdk/internal/misc/Signal",
            "<init>",
            "(Ljava/lang/String;)V",
            &[
                Value::Object(Some(signal_obj)),
                Value::Object(Some(name_str)),
            ],
        );
        assert!(result.is_ok());

        let num_result = call_native(
            &reg,
            &mut ctx,
            "jdk/internal/misc/Signal",
            "getNumber",
            "()I",
            &[Value::Object(Some(signal_obj))],
        );
        assert_eq!(num_result.unwrap(), Some(Value::Int(15)));
    }
}

/// gc-common w10-a: `Signal.handle` / `Signal.raise` keep one handler table
/// per VM, read the signal number by name, and call the handler with its own
/// package's descriptor.
#[cfg(test)]
mod w10a_signal_handler_vm_isolation_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::cell::RefCell;

    const VM_A: usize = 0xA10_5A01;
    const VM_B: usize = 0xA10_5A02;

    struct ForgetBoth;
    impl Drop for ForgetBoth {
        fn drop(&mut self) {
            forget_vm_signal_handlers(VM_A);
            forget_vm_signal_handlers(VM_B);
        }
    }

    thread_local! {
        /// `(receiver address, descriptor)` of every `handle` upcall.
        static RAISED: RefCell<Vec<(usize, String)>> = const { RefCell::new(Vec::new()) };
    }

    fn record_handle(
        _ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method: &str,
        descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method != "handle" {
            return None;
        }
        RAISED.with(|r| {
            r.borrow_mut()
                .push((receiver.as_ptr() as usize, descriptor.to_string()))
        });
        Some(Ok(None))
    }

    fn registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_signal_natives(&mut r);
        r
    }

    fn call(
        reg: &NativeMethodRegistry,
        ctx: &mut MockNativeContext,
        class: &str,
        method: &str,
        desc: &str,
        args: &[Value],
    ) -> Option<Value> {
        let cb = reg
            .find(class, method, desc)
            .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
        cb(ctx, args).ok().flatten()
    }

    fn signal(reg: &NativeMethodRegistry, ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(class).expect("declare Signal");
        let sig = ctx.alloc_object(cid, 4);
        let name = ctx.create_string("INT");
        call(
            reg,
            ctx,
            class,
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sig)), Value::Object(Some(name))],
        );
        sig
    }

    const SUN_HANDLE: &str = "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;";

    #[test]
    fn signal_handlers_are_per_vm() {
        let _forget = ForgetBoth;
        let reg = registry();
        let mut a = MockNativeContext::new();
        a.set_vm_identity(VM_A);
        a.set_invoke_virtual_hook(record_handle);
        let mut b = MockNativeContext::new();
        b.set_vm_identity(VM_B);
        b.set_invoke_virtual_hook(record_handle);

        let sig_a = signal(&reg, &mut a, "sun/misc/Signal");
        let sig_b = signal(&reg, &mut b, "sun/misc/Signal");
        let ha = a.fresh_object_ref();
        let hb = b.fresh_object_ref();

        let sig_class = "sun/misc/Signal";
        let args_a = [Value::Object(Some(sig_a)), Value::Object(Some(ha))];
        let prev = call(&reg, &mut a, sig_class, "handle", SUN_HANDLE, &args_a);
        assert_eq!(prev, Some(Value::Object(None)));
        let args_b = [Value::Object(Some(sig_b)), Value::Object(Some(hb))];
        let prev = call(&reg, &mut b, sig_class, "handle", SUN_HANDLE, &args_b);
        assert_eq!(
            prev,
            Some(Value::Object(None)),
            "VM B's first handle(SIGINT) must not return VM A's handler"
        );

        RAISED.with(|r| r.borrow_mut().clear());
        let raise = "(Lsun/misc/Signal;)V";
        call(
            &reg,
            &mut b,
            sig_class,
            "raise",
            raise,
            &[Value::Object(Some(sig_b))],
        );
        call(
            &reg,
            &mut a,
            sig_class,
            "raise",
            raise,
            &[Value::Object(Some(sig_a))],
        );
        let raised: Vec<usize> = RAISED.with(|r| r.borrow().iter().map(|(p, _)| *p).collect());
        assert_eq!(
            raised,
            vec![hb.as_ptr() as usize, ha.as_ptr() as usize],
            "each VM's raise must call its own handler"
        );

        forget_vm_signal_handlers(VM_A);
        let clear_b = [Value::Object(Some(sig_b)), Value::Object(None)];
        let prev = call(&reg, &mut b, sig_class, "handle", SUN_HANDLE, &clear_b);
        assert_eq!(
            prev,
            Some(Value::Object(Some(hb))),
            "A's teardown left B's handler"
        );
    }

    #[test]
    fn a_jdk_internal_signal_handler_is_called_with_its_own_descriptor() {
        const VM: usize = 0xA10_5A03;
        struct Forget;
        impl Drop for Forget {
            fn drop(&mut self) {
                forget_vm_signal_handlers(VM);
            }
        }
        let _forget = Forget;
        let reg = registry();
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(VM);
        ctx.set_invoke_virtual_hook(record_handle);
        let class = "jdk/internal/misc/Signal";
        let sig = signal(&reg, &mut ctx, class);
        let h = ctx.fresh_object_ref();
        let handle = "(Ljdk/internal/misc/Signal;Ljdk/internal/misc/SignalHandler;)\
                      Ljdk/internal/misc/SignalHandler;";
        let args = [Value::Object(Some(sig)), Value::Object(Some(h))];
        call(&reg, &mut ctx, class, "handle", handle, &args);

        RAISED.with(|r| r.borrow_mut().clear());
        let raise = "(Ljdk/internal/misc/Signal;)V";
        call(
            &reg,
            &mut ctx,
            class,
            "raise",
            raise,
            &[Value::Object(Some(sig))],
        );
        let raised = RAISED.with(|r| r.borrow().clone());
        assert_eq!(
            raised,
            vec![(
                h.as_ptr() as usize,
                "(Ljdk/internal/misc/Signal;)V".to_string()
            )],
            "a jdk.internal.misc handler must be called with the jdk.internal.misc descriptor"
        );
    }

    /// `Beans.setDesignTime` / `setGuiAvailable` in one VM leave the other
    /// VM's answers alone.
    #[test]
    fn beans_state_is_per_vm() {
        const VM_C: usize = 0xA10_5A04;
        const VM_D: usize = 0xA10_5A05;
        struct Forget;
        impl Drop for Forget {
            fn drop(&mut self) {
                forget_vm_beans_state(VM_C);
                forget_vm_beans_state(VM_D);
            }
        }
        let _forget = Forget;
        let mut r = NativeMethodRegistry::new();
        register_deprecated_internal_natives(&mut r);
        let beans = "java/beans/Beans";
        let mut c = MockNativeContext::new();
        c.set_vm_identity(VM_C);
        let mut d = MockNativeContext::new();
        d.set_vm_identity(VM_D);

        call(&r, &mut c, beans, "setDesignTime", "(Z)V", &[Value::Int(1)]);
        call(
            &r,
            &mut c,
            beans,
            "setGuiAvailable",
            "(Z)V",
            &[Value::Int(1)],
        );
        assert_eq!(
            call(&r, &mut c, beans, "isDesignTime", "()Z", &[]),
            Some(Value::Int(1))
        );
        assert_eq!(
            call(&r, &mut c, beans, "isGuiAvailable", "()Z", &[]),
            Some(Value::Int(1))
        );
        assert_eq!(
            call(&r, &mut d, beans, "isDesignTime", "()Z", &[]),
            Some(Value::Int(0)),
            "VM C's setDesignTime(true) must not reach VM D"
        );
        assert_eq!(
            call(&r, &mut d, beans, "isGuiAvailable", "()Z", &[]),
            Some(Value::Int(0))
        );

        forget_vm_beans_state(VM_C);
        assert_eq!(
            call(&r, &mut c, beans, "isDesignTime", "()Z", &[]),
            Some(Value::Int(0))
        );
    }
}

/// SBR wave 5 FJP2: `jdk.internal.misc.Signal.handle0` answers the PREVIOUS
/// disposition per VM (HotSpot's `JVM_RegisterSignal`), refuses the VM's own
/// signals with `-1`, and `raise0` dispatches only a signal whose disposition
/// is the Java handler (`2`).
#[cfg(test)]
mod fjp2_signal_disposition_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    const VM_D: usize = 0xF2_5A01;
    const VM_E: usize = 0xF2_5A02;

    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_signal_handlers(VM_D);
            forget_vm_signal_handlers(VM_E);
        }
    }

    fn handle0(ctx: &mut MockNativeContext, sig: i32, new_h: i64) -> i64 {
        match native_signal_handle0(ctx, &[Value::Int(sig), Value::Long(new_h)]) {
            Ok(Some(Value::Long(v))) => v,
            other => panic!("handle0 answered {other:?}"),
        }
    }

    #[test]
    fn handle0_answers_the_previous_disposition_per_vm_and_refuses_vm_signals() {
        let _forget = Forget;
        let mut d = MockNativeContext::new();
        d.set_vm_identity(VM_D);
        let mut e = MockNativeContext::new();
        e.set_vm_identity(VM_E);
        // USR1 (10): the first answer is the process's OS disposition, whatever
        // it is; after that the table answers.
        assert_eq!(handle0(&mut d, 10, 2), signal_os_disposition(10));
        assert_eq!(
            handle0(&mut d, 10, 0),
            2,
            "a re-registration sees the Java handler"
        );
        assert_eq!(handle0(&mut d, 10, 2), 0);
        // Another VM's table is its own.
        assert_eq!(handle0(&mut e, 10, 1), signal_os_disposition(10));
        assert_eq!(handle0(&mut d, 10, 2), 2);
        // HotSpot 25 refuses these (`Signal already used by VM or OS`):
        // QUIT, ILL, FPE, SEGV on POSIX; FPE and BREAK on Windows (wave 100).
        let reserved: &[i32] = if cfg!(windows) {
            &[8, 21]
        } else {
            &[3, 4, 8, 11]
        };
        for &sig in reserved {
            assert_eq!(handle0(&mut d, sig, 2), -1, "signal {sig}");
        }
    }

    #[test]
    fn shutdown_ignores_are_sticky_and_xrs_registration_is_per_vm() {
        let mut d = MockNativeContext::new();
        d.set_vm_identity(0xF2_5A03);
        let mut e = MockNativeContext::new();
        e.set_vm_identity(0xF2_5A04);
        e.reduce_signal_usage = true;
        // Windows has no HUP, and its `JVM_RegisterSignal` does not keep an
        // ignore: the second registration replaces it (wave 100, lane L1).
        let shutdown: &[i32] = if cfg!(windows) { &[2, 15] } else { &[1, 2, 15] };
        let after_reinstall = if cfg!(windows) {
            HostSignalDisposition::Java
        } else {
            HostSignalDisposition::Ignore
        };
        for &sig in shutdown {
            handle0(&mut d, sig, 1);
            assert_eq!(handle0(&mut d, sig, 2), 1);
            assert_eq!(host_signal_disposition(&d, sig), after_reinstall);
            assert_eq!(handle0(&mut e, sig, 1), -1);
            assert_eq!(
                host_signal_disposition(&e, sig),
                HostSignalDisposition::Shutdown
            );
        }
        assert_ne!(handle0(&mut e, 10, 2), -1);
        assert_eq!(host_signal_disposition(&e, 10), HostSignalDisposition::Java);
        forget_vm_signal_handlers(0xF2_5A03);
        forget_vm_signal_handlers(0xF2_5A04);
    }

    #[test]
    fn raise0_without_a_java_handler_runs_nothing() {
        let _forget = Forget;
        let mut d = MockNativeContext::new();
        d.set_vm_identity(VM_D);
        // No row, then SIG_IGN: neither is the Java handler, so no `dispatch`
        // is invoked.
        assert!(matches!(
            native_signal_raise0(&mut d, &[Value::Int(14)]),
            Ok(None)
        ));
        handle0(&mut d, 14, 1);
        assert!(matches!(
            native_signal_raise0(&mut d, &[Value::Int(14)]),
            Ok(None)
        ));
    }
}

#[cfg(test)]
mod caller_frame_index_tests {
    use super::*;
    use cratonvm_native_api::StackTraceEntry;
    use std::sync::Arc;

    fn entries(names: &[&str]) -> Vec<StackTraceEntry> {
        names
            .iter()
            .map(|n| StackTraceEntry {
                class_name: Arc::from(*n),
                method_name: Arc::from("m"),
                method_descriptor: None,
                source_file: None,
                line_number: -1,
                byte_code_index: 0,
                class_id: None,
                method_index: None,
            })
            .collect()
    }

    fn idx(names: &[&str], whole: bool) -> Option<Option<usize>> {
        caller_frame_index(&entries(names), whole).ok()
    }

    /// The answer of the whole-trace walk this replaced, spelled out: the entry
    /// below the top, skipping stream / Spliterator / ServiceLoader plumbing
    /// outward, the outermost entry when the plumbing runs out.
    fn whole_trace_answer(names: &[&str]) -> Option<usize> {
        let n = names.len();
        match n {
            0 => None,
            1 => Some(0),
            _ => {
                let mut i = n - 2;
                loop {
                    let name = names[i];
                    let plumbing = name.starts_with("java/util/stream/")
                        || name == "java/util/ServiceLoader"
                        || (name.starts_with("java/util/") && name.contains("Spliterator"));
                    if !plumbing || i == 0 {
                        return Some(i);
                    }
                    i -= 1;
                }
            }
        }
    }

    #[test]
    fn the_whole_trace_answers_as_before() {
        for names in [
            vec![],
            vec!["A"],
            vec!["A", "B"],
            vec!["A", "B", "C"],
            vec!["A", "java/util/ServiceLoader", "B"],
            vec![
                "A",
                "java/util/stream/ForEachOps$ForEachOp$OfRef",
                "java/util/ServiceLoader",
                "B",
            ],
            vec!["java/util/stream/X", "java/util/ServiceLoader", "B"],
            vec!["A", "java/util/Spliterators$X", "java/util/ServiceLoader"],
        ] {
            assert_eq!(
                idx(&names, true),
                Some(whole_trace_answer(&names)),
                "{names:?}"
            );
        }
    }

    /// A tail is asked for more exactly when the walk ends inside it or the
    /// tail is too short to say what lies below it.
    #[test]
    fn a_tail_that_ends_inside_the_plumbing_asks_for_more() {
        assert!(caller_frame_index(&entries(&[]), false).is_err());
        assert!(caller_frame_index(&entries(&["A"]), false).is_err());
        assert_eq!(idx(&["A", "B"], false), Some(Some(0)));
        // The walk settles on a non-plumbing frame inside the tail.
        assert_eq!(idx(&["X", "A", "B"], false), Some(Some(1)));
        // Plumbing all the way to the top of the tail: more is needed, unless
        // that IS the whole trace.
        let names = ["java/util/stream/X", "java/util/ServiceLoader", "B"];
        assert!(caller_frame_index(&entries(&names), false).is_err());
        assert_eq!(idx(&names, true), Some(Some(0)));
    }

    /// For every whole trace and every tail length, a tail that answers answers
    /// the whole trace's index (shifted into the tail); one that cannot settle
    /// says so.
    #[test]
    fn every_tail_agrees_with_the_whole_trace() {
        let all = [
            "M",
            "A",
            "java/util/stream/S",
            "java/util/ServiceLoader",
            "java/util/stream/T",
            "B",
            "C",
        ];
        for len in 0..=all.len() {
            let whole = &all[..len];
            let want = whole_trace_answer(whole);
            for tail_len in 0..=len {
                let tail = &whole[len - tail_len..];
                let is_whole = tail_len == len;
                match caller_frame_index(&entries(tail), is_whole) {
                    Ok(Some(i)) => {
                        assert_eq!(Some(len - tail_len + i), want, "{whole:?} tail {tail_len}")
                    }
                    Ok(None) => assert_eq!(want, None, "{whole:?} tail {tail_len}"),
                    Err(NeedsWiderTail) => assert!(!is_whole, "{whole:?} tail {tail_len}"),
                }
            }
        }
    }
}
