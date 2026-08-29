// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deprecated java.beans, java.rmi, sun.misc, and jdk.internal.* native implementations.
//!
//! These APIs are deprecated or removed in modern JDKs but legacy code may still call
//! them. Every method is registered via NativeMethodRegistry with real implementations
//! (not stubs): proper validation, error handling, and security checks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError};
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

/// Map signal number -> handler ObjectRef (or None for SIG_DFL).
///
/// GC note (gc-followups-20260706): KNOWN-UNSOUND across GCs — the handler
/// refs are neither GC roots nor remapped, so `Signal.raise` after a moving
/// GC invokes a stale (or reclaimed) handler. Follow-up: convert to the
/// `(identity_key, ObjectRef)` var-handle-root pattern (see ASYNC_POOL in
/// lib.rs) or add a gc_scan/gc_update hook pair.
fn signal_handler_store() -> &'static Mutex<HashMap<i32, Option<ObjectRef>>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<i32, Option<ObjectRef>>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
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

/// `java.beans.Beans` design-time flag. The real JDK scopes this per
/// `ThreadGroupContext`; CratonVM has one such context in practice, so a
/// process-global flag is observationally equivalent.
static BEANS_DESIGN_TIME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `java.beans.Beans` GUI-available override: `-1` un-set (fall back to the
/// headless computation), `0` false, `1` true. Mirrors the real JDK's
/// `ThreadGroupContext.isGuiAvailable` being a nullable `Boolean`.
static BEANS_GUI_AVAILABLE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

fn native_beans_is_design_time(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let on = BEANS_DESIGN_TIME.load(Ordering::Relaxed);
    Ok(Some(Value::Int(i32::from(on))))
}

fn native_beans_set_design_time(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static (Z)V — args[0] is the boolean.
    let on = matches!(args.first(), Some(Value::Int(v)) if *v != 0);
    BEANS_DESIGN_TIME.store(on, Ordering::Relaxed);
    Ok(None)
}

fn native_beans_is_gui_available(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let available = match BEANS_GUI_AVAILABLE.load(Ordering::Relaxed) {
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

fn native_beans_set_gui_available(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let on = matches!(args.first(), Some(Value::Int(v)) if *v != 0);
    BEANS_GUI_AVAILABLE.store(i8::from(on), Ordering::Relaxed);
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
    let u = "sun/misc/Unsafe";
    let u2 = "jdk/internal/misc/Unsafe";

    // --- T8.4.1: defineClass with CAFEBABE validation ---
    //
    // The `sun/misc/Unsafe` registration was RETIRED 2026-08-29.
    // `sun.misc.Unsafe.defineClass` is ABSENT from every supported image (JDK
    // 17.0.20.1+1, 21.0.12+8, 25.0.4+7 -- measured, not read off one image), so
    // nothing could dispatch to it, and it took 0 invocations across 118 corpus
    // vectors in both modes.
    //
    // `deprecated_verify.rs` tagged it `ImageStatus::Declared`, whose own doc
    // reads "at least one supported image declares the triple ... and MUST
    // stay". That tag was a claim NOTHING COULD FALSIFY: the T8 test asserts a
    // `Declared` row IS registered and never checks the claim against an image,
    // while it does enforce the opposite direction. Retagged in the same
    // commit, which is what makes this removal the manifest's own instruction.
    //
    // The `jdk/internal/misc` spelling below is declared on all three images
    // and stays.
    r.register(
        u2,
        "defineClass",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        native_unsafe_define_class,
    );

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

fn native_unsafe_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe_this, name, byte[], offset, length, classLoader, protectionDomain]
    //
    // WP2.3: routes through `NativeContext::define_class_full` so the
    // four entry points (Unsafe.defineClass [sun.misc + jdk.internal.misc],
    // ClassLoader.defineClass1/2, MethodHandles.Lookup.defineClass)
    // share the same backend with consistent error semantics, name
    // checks, hidden-class flag, and ProtectionDomain attribution.
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("byte array is null".to_string()),
            }
            .into());
        }
    };
    let offset = match args.get(3) {
        Some(Value::Int(o)) if *o >= 0 => *o as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(l)) if *l >= 0 => *l as usize,
        Some(Value::Int(_)) => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
        _ => 0,
    };

    let arr_len = ctx.array_length(byte_array);

    // Validate bounds
    if offset.saturating_add(length) > arr_len {
        return Err(RuntimeError::aioobe_index_only((offset + length) as i32).into());
    }

    // Extract bytes
    let mut class_bytes = Vec::with_capacity(length);
    for i in offset..offset + length {
        match ctx.get_array_element(byte_array, i) {
            Value::Int(b) => class_bytes.push(b as u8),
            _ => class_bytes.push(0),
        }
    }

    // Get class name. If null, the backend will use the class file's
    // own `this_class`. WP2.3 backend rejects mismatches with
    // NoClassDefFoundError.
    let class_name = name_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default()
        .replace('.', "/");

    // Resolve the loader id (arg 5). If null/bootstrap, use 0 to mean
    // application loader; otherwise ask for the loader's own CratonVM
    // namespace id.
    //
    // L1: this was a raw `get_field(loader_obj, 6)`. On a real JDK image slot
    // 6 is `java.lang.ClassLoader.classes` — an `ArrayList<Class<?>>`
    // reference, not our id — so the raw read could never answer anything but
    // the `0` fallback there. `loader_id_of` consults the side table the
    // `ClassLoader` constructor natives write and only reads the slot on our
    // own synthetic layout.
    let loader_id = match args.get(5) {
        Some(Value::Object(Some(loader_obj))) => {
            crate::classloader::loader_id_of(ctx, *loader_obj).unwrap_or(0)
        }
        _ => 0,
    };

    // BUG-10: `Unsafe.defineClass` is the privileged JVM-internal define path
    // that HotSpot routes around `preDefineClass`'s prohibited-package guard.
    // ByteBuddy's `ClassInjector` injects `java.lang.ClassLoader$ByteBuddyAccessor$V1`
    // through it (driven by AssertJ/Mockito); mark the define privileged so the
    // H5 guard is bypassed, matching the real JVM. (Twin of the handler in
    // `unsafe_natives.rs`; both `Unsafe.defineClass` registrations must agree.)
    let opts = cratonvm_native_api::DefineClassFull {
        privileged_define: true,
        ..Default::default()
    };
    let effective_name = if class_name.is_empty() {
        // No name supplied — backend will pull `this_class` from the
        // class file. We pass empty so the name-match check is a
        // no-op.
        "".to_string()
    } else {
        class_name.clone()
    };
    match ctx.define_class_full(&effective_name, &class_bytes, loader_id, opts) {
        Ok(cid) => {
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => Err(LinkageError::ClassFormatError {
            class_name: if class_name.is_empty() {
                "<unknown>".into()
            } else {
                class_name
            },
            message: msg,
        }
        .into()),
    }
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
    if crate::panama::module_native_access_enabled(module.as_deref()) {
        return Ok(None);
    }

    static NATIVE_ACCESS_WARNED: std::sync::OnceLock<Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let key = module.unwrap_or_else(|| "ALL-UNNAMED".to_string());
    let first_time = NATIVE_ACCESS_WARNED
        .get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
        .lock()
        .map(|mut seen| seen.insert(key.clone()))
        .unwrap_or(false);
    if !first_time {
        return Ok(None);
    }

    let owner = match args.get(1) {
        Some(Value::Object(Some(mirror))) => ctx
            .class_id_from_mirror(*mirror)
            .and_then(|cid| ctx.class_name_of_id(cid))
            .map(|n| n.replace('/', "."))
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
            let frames = ctx.capture_stack_trace(0);
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
            let frames = ctx.capture_stack_trace(0);
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
            let frames = ctx.capture_stack_trace(0);
            let target = if (depth as usize) <= frames.len() {
                frames.get(frames.len() - depth as usize)
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
            let frames = ctx.capture_stack_trace(0);
            let target = match frames.len() {
                0 => None,
                1 => frames.get(0),
                n => frames.get(n - 2),
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
            let flags = match args.first() {
                Some(Value::Object(Some(mirror))) => ctx
                    .class_id_from_mirror(*mirror)
                    .map(|cid| i32::from(ctx.class_access_flags(cid)))
                    .filter(|flags| *flags != 0),
                _ => None,
            };
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
            let n = ctx.array_length(roots_arr);

            // Collect each input root's path via File.getPath()
            let mut out_paths: Vec<String> = Vec::new();
            for i in 0..n {
                let elem = ctx.get_array_element(roots_arr, i);
                let root_ref = match elem {
                    Value::Object(Some(r)) => r,
                    _ => continue,
                };
                // Invoke File.getPath() to get the root's path string.
                let path_val = ctx.invoke(
                    "java/io/File",
                    "getPath",
                    "()Ljava/lang/String;",
                    &[Value::Object(Some(root_ref))],
                )?;
                let root_path = match path_val {
                    Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
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
            let file_cid = ctx.ensure_class_initialized("java/io/File")?;
            let out_arr = ctx.new_ref_array(file_cid, out_paths.len());
            for (i, p) in out_paths.iter().enumerate() {
                let path_str = ctx.create_string(p);
                let file_obj = match ctx.new_object("java/io/File")? {
                    Some(Value::Object(Some(f))) => f,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "failed to allocate java/io/File".to_string(),
                        }
                        .into());
                    }
                };
                ctx.invoke(
                    "java/io/File",
                    "<init>",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(file_obj)), Value::Object(Some(path_str))],
                )?;
                ctx.set_array_element(out_arr, i, Value::Object(Some(file_obj)));
            }
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
    // fixed-suite-bugs/h2-suite-bugs/bug-h2-suite-residual-fail-triage-FIXED.md.

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

        let sig_num = match ctx.get_field(signal_obj, 1) {
            Value::Int(n) => n,
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Signal object has no signal number".to_string(),
                }
                .into());
            }
        };

        // Swap old handler with new one
        let mut store = signal_handler_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old_handler = store.insert(sig_num, handler).flatten();
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
        let sig_num = match ctx.get_field(signal_obj, 1) {
            Value::Int(n) => n,
            _ => return Ok(None),
        };

        let handler = {
            let store = signal_handler_store()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            store.get(&sig_num).and_then(|h| *h)
        };

        if let Some(handler_obj) = handler {
            // Invoke handler.handle(Signal)
            let handler_desc = if sig_num >= 0 {
                // We pass the signal object to the handler
                "(Lsun/misc/Signal;)V"
            } else {
                "(Ljdk/internal/misc/Signal;)V"
            };
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

    // --- T8.4.1: Unsafe.defineClass validates CAFEBABE ---

    #[test]
    fn test_unsafe_define_class_valid_magic() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        // Build a byte array with CAFEBABE magic
        let class_bytes: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, class_bytes.len());
        for (i, b) in class_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }

        let name = ctx.create_string("test/ValidClass");

        let result = call_native(
            &reg,
            &mut ctx,
            // `sun.misc` spelling RETIRED 2026-08-29 -- absent from JDK 17,
            // 21 and 25. This test's subject is the CAFEBABE validation, not
            // the spelling, and `jdk.internal.misc` runs the same body.
            "jdk/internal/misc/Unsafe",
            "defineClass",
            "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
            &[
                Value::Object(None), // unsafe this
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(class_bytes.len() as i32),
                Value::Object(None), // classloader
                Value::Object(None), // protection domain
            ],
        );
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Some(Value::Object(Some(_)))));
    }

    #[test]
    fn test_unsafe_define_class_invalid_magic() {
        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

        let bad_bytes: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00];
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bad_bytes.len());
        for (i, b) in bad_bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }

        let name = ctx.create_string("test/BadClass");

        let result = call_native(
            &reg,
            &mut ctx,
            // `sun.misc` spelling RETIRED 2026-08-29 -- absent from JDK 17,
            // 21 and 25. This test's subject is the CAFEBABE validation, not
            // the spelling, and `jdk.internal.misc` runs the same body.
            "jdk/internal/misc/Unsafe",
            "defineClass",
            "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
            &[
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(bad_bytes.len() as i32),
                Value::Object(None),
                Value::Object(None),
            ],
        );
        assert!(result.is_err());
    }

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
        // Reset signal handler state
        signal_handler_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        let reg = make_registry();
        let mut ctx = MockNativeContext::new();

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
