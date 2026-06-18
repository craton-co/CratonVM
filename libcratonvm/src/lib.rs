// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! # `libcratonvm` — C-ABI embedding surface for CratonVM
//!
//! This crate produces a `cdylib`/`staticlib` (`libcratonvm.so` /
//! `cratonvm.dll` / `libcratonvm.a`) that lets a non-Rust host process create
//! and drive a CratonVM JVM through the standard **JNI Invocation API** — the
//! same three exported entry points (`JNI_CreateJavaVM`,
//! `JNI_GetDefaultJavaVMInitArgs`, `JNI_GetCreatedJavaVMs`) that a host calls
//! when it loads a `libjvm.so`/`jvm.dll`.
//!
//! ## What this is (Increment 1 / Layer 1 C-ABI)
//!
//! This is the **JNI Invocation API** layer of the design in
//! `docs/feature-designs/embedding-api.md` (Layer 3 in the doc's numbering —
//! the `libjvm`-substitute bootstrap entry points). It reuses, verbatim, the
//! per-`Vm` `extern "C"` function tables that already live in
//! `cratonvm-vm`'s `native::jni` module:
//!
//! * [`cratonvm_vm::native::jni::get_java_vm`] returns the process-global
//!   `JavaVM*` whose invocation function table already wires `DestroyJavaVM`,
//!   `AttachCurrentThread`, `DetachCurrentThread`, `GetEnv`, and
//!   `AttachCurrentThreadAsDaemon`.
//! * [`cratonvm_vm::native::jni::get_jni_env`] returns the per-thread `JNIEnv*`
//!   backed by the 234-slot `JNIEnv` function table (`FindClass`,
//!   `GetStaticMethodID`, `CallStaticVoidMethod`, …) already used by native
//!   methods.
//! * [`cratonvm_vm::native::jni::set_jni_context_arc`] publishes the
//!   `Arc<SharedVm>` into the calling thread's TLS so the `JNIEnv` table
//!   functions can resolve the live VM — without this, an embedder-driven
//!   `FindClass` would have no VM to talk to.
//!
//! The *only* genuinely new machinery here is the **bootstrap** path
//! (`JNI_CreateJavaVM` → `Vm::new` → run init phases → publish the tables) and
//! the **process-global VM registry** that `JNI_GetCreatedJavaVMs` reports.
//! That registry is the gap called out in the design doc; the function tables
//! were already complete.
//!
//! ## Threading / lifecycle contract (mirrors HotSpot)
//!
//! * **One VM per process.** HotSpot allows at most one VM; we mirror that.
//!   A second `JNI_CreateJavaVM` returns [`JNI_EEXIST`].
//! * The thread that calls `JNI_CreateJavaVM` becomes the VM's main thread and
//!   has its JNI TLS context set on return — it may immediately use the
//!   returned `JNIEnv*`.
//! * Other host threads must `AttachCurrentThread` (slot 4 of the invocation
//!   table) before using a `JNIEnv*`.
//!
//! ## Panic safety
//!
//! A Rust panic unwinding across the `extern "C"` boundary into C is undefined
//! behaviour. Every `#[no_mangle] pub extern "C"` entry point in this crate
//! wraps its body in [`std::panic::catch_unwind`] and converts a caught panic
//! to [`JNI_ERR`].

#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::native::jni::{get_java_vm, get_jni_env, set_jni_context_arc};
use cratonvm_vm::vm::Vm;

// ---------------------------------------------------------------------------
// JNI integer / handle types (re-exported from the vm crate so the C ABI is
// identical to the function-table side).
// ---------------------------------------------------------------------------

pub use cratonvm_vm::native::jni::{JInt, JSize, JavaVM, JNIEnv};

/// `jni.h`: success.
pub const JNI_OK: JInt = 0;
/// `jni.h`: unknown / generic error.
pub const JNI_ERR: JInt = -1;
/// `jni.h`: thread detached from the VM.
pub const JNI_EDETACHED: JInt = -2;
/// `jni.h`: JNI version error.
pub const JNI_EVERSION: JInt = -3;
/// `jni.h`: not enough memory.
pub const JNI_ENOMEM: JInt = -4;
/// `jni.h`: VM already created (we enforce HotSpot's one-VM-per-process).
pub const JNI_EEXIST: JInt = -5;
/// `jni.h`: invalid argument.
pub const JNI_EINVAL: JInt = -6;

/// The JNI version this VM reports / requests by default.
///
/// Kept in lockstep with `cratonvm_vm::native::jni::JNI_VERSION_1_8` so the
/// Invocation API and the function-table side agree on a single value.
pub const JNI_VERSION: JInt = cratonvm_vm::native::jni::JNI_VERSION_1_8;

// ---------------------------------------------------------------------------
// JNI Invocation-API argument structs (jni.h layout, `#[repr(C)]`).
//
// These are the exact ABI shapes a `libjvm` host passes by pointer. They do
// not exist in the vm crate (it only had the *function tables*, not the
// *creation argument* structs), so they are defined here as the public C ABI.
// ---------------------------------------------------------------------------

/// One `-X…` / `-D…` / agent option, as passed in [`JavaVMInitArgs`].
///
/// Matches `JavaVMOption` from `jni.h`:
/// ```c
/// typedef struct JavaVMOption {
///     char *optionString;
///     void *extraInfo;
/// } JavaVMOption;
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JavaVMOption {
    /// NUL-terminated option string, e.g. `b"-Xmx256m\0"` or `b"-cp\0"` style
    /// (CratonVM accepts the HotSpot `-Xmx`, `-cp`/`-classpath`, `-D<k>=<v>`
    /// forms; unrecognised options are ignored when `ignoreUnrecognized` is
    /// non-zero).
    pub option_string: *mut c_char,
    /// Reserved / abort+exit hook pointer in real HotSpot. Unused here.
    pub extra_info: *mut c_void,
}

/// Init args for [`JNI_CreateJavaVM`], matching `JavaVMInitArgs` from `jni.h`:
/// ```c
/// typedef struct JavaVMInitArgs {
///     jint version;
///     jint nOptions;
///     JavaVMOption *options;
///     jboolean ignoreUnrecognized;
/// } JavaVMInitArgs;
/// ```
#[repr(C)]
pub struct JavaVMInitArgs {
    /// Requested JNI version (e.g. `JNI_VERSION`).
    pub version: JInt,
    /// Number of entries in `options`.
    pub n_options: JInt,
    /// Pointer to an array of `n_options` [`JavaVMOption`]s (may be null when
    /// `n_options == 0`).
    pub options: *mut JavaVMOption,
    /// When non-zero, unrecognised options are ignored rather than rejected.
    pub ignore_unrecognized: u8,
}

// ---------------------------------------------------------------------------
// Process-global VM registry.
//
// HotSpot permits at most one VM per process; we mirror that. The created `Vm`
// is parked here for its whole life so the `Arc<SharedVm>` it owns (and the
// heap/threads behind it) stay alive after `JNI_CreateJavaVM` returns. The
// `JavaVM*`/`JNIEnv*` we hand back point at the process-global function-table
// statics in `jni.rs`, which are themselves leaked singletons — so the handles
// remain valid for the life of the process.
// ---------------------------------------------------------------------------

/// A `Vm` parked for the life of the process.
///
/// `Vm` owns an `Arc<SharedVm>` (which the design pins as `Send + Sync`) and a
/// `JvmThread` for the main thread. The `JvmThread` is what makes `Vm`
/// not-automatically-`Send`, but the parked `Vm` here is **never accessed
/// across threads through this slot**: it exists only to keep the
/// `Arc<SharedVm>` (and the heap/threads behind it) alive after
/// `JNI_CreateJavaVM` returns. All live VM access goes through the per-thread
/// `Arc<SharedVm>` published by `set_jni_context_arc`. This is the same
/// reasoning as the `SendPtr` wrapper in `vm/src/native/jni.rs`.
struct ParkedVm(Box<Vm>);

// SAFETY: see `ParkedVm` doc — the parked `Vm` is only constructed under the
// registry mutex and then held; it is not driven from another thread via this
// slot, so moving the box between threads (which `Mutex`/`static` may imply)
// does not create a data race on the `JvmThread`.
unsafe impl Send for ParkedVm {}

/// Holds the single created VM. `None` until the first `JNI_CreateJavaVM`.
/// `Mutex` makes the create/registry transition atomic against a racing second
/// `JNI_CreateJavaVM` (one-VM-per-process).
static CREATED_VM: Mutex<Option<ParkedVm>> = Mutex::new(None);

/// `true` once a VM has been created (drives `JNI_GetCreatedJavaVMs` and the
/// one-VM-per-process guard).
fn vm_exists() -> bool {
    CREATED_VM
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// JNI_GetDefaultJavaVMInitArgs
// ---------------------------------------------------------------------------

/// `jint JNI_GetDefaultJavaVMInitArgs(void *args)`
///
/// Populates `args->version` with the version this VM supports and zeroes the
/// option fields. A host calls this first to discover the supported version
/// before building its own [`JavaVMInitArgs`].
///
/// # Safety
/// `args` must point to a writable [`JavaVMInitArgs`] (or be null, in which
/// case [`JNI_ERR`] is returned).
#[no_mangle]
pub extern "C" fn JNI_GetDefaultJavaVMInitArgs(args: *mut c_void) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if args.is_null() {
            return JNI_ERR;
        }
        // SAFETY: caller contract — `args` is a valid `*mut JavaVMInitArgs`.
        let init = unsafe { &mut *(args as *mut JavaVMInitArgs) };
        // HotSpot's GetDefaultJavaVMInitArgs returns JNI_EVERSION if the caller
        // pre-set a version it doesn't support, JNI_OK otherwise, and always
        // writes back the supported version. We accept anything and report
        // ours.
        init.version = JNI_VERSION;
        init.n_options = 0;
        init.options = std::ptr::null_mut();
        init.ignore_unrecognized = 0;
        JNI_OK
    }))
    .unwrap_or(JNI_ERR)
}

// ---------------------------------------------------------------------------
// JNI_GetCreatedJavaVMs
// ---------------------------------------------------------------------------

/// `jint JNI_GetCreatedJavaVMs(JavaVM **vmBuf, jsize bufLen, jsize *nVMs)`
///
/// Reports the VMs created in this process — at most one, mirroring HotSpot.
/// Writes the live `JavaVM*` into `vm_buf[0]` (if `buf_len >= 1` and one
/// exists) and the count into `*n_vms`.
///
/// # Safety
/// `vm_buf` must point to storage for at least `buf_len` `JavaVM` values (or be
/// null when `buf_len == 0`); `n_vms` must be writable or null.
#[no_mangle]
pub extern "C" fn JNI_GetCreatedJavaVMs(
    vm_buf: *mut JavaVM,
    buf_len: JSize,
    n_vms: *mut JSize,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        let count: JSize = if vm_exists() { 1 } else { 0 };
        if !n_vms.is_null() {
            // SAFETY: caller contract — `n_vms` is writable when non-null.
            unsafe { *n_vms = count };
        }
        if count == 1 && buf_len >= 1 && !vm_buf.is_null() {
            // SAFETY: `buf_len >= 1` and `vm_buf` non-null per the checks above.
            unsafe { *vm_buf = get_java_vm() };
        }
        JNI_OK
    }))
    .unwrap_or(JNI_ERR)
}

// ---------------------------------------------------------------------------
// JNI_CreateJavaVM
// ---------------------------------------------------------------------------

/// Parse a `JavaVMInitArgs` into a [`VmConfig`].
///
/// Recognises the common HotSpot option forms an embedder is likely to pass:
/// `-Xmx<size>`, `-cp`/`-classpath`/`--class-path <path>` (the path may be the
/// next option or glued as `-cp=<path>`), and `-D<key>=<value>`. Unrecognised
/// options are ignored (matching `ignoreUnrecognized`-friendly behaviour);
/// they are never fatal in this first increment.
///
/// # Safety
/// `args` must be a valid `*const JavaVMInitArgs` with `options` pointing at
/// `n_options` valid, NUL-terminated [`JavaVMOption`] strings.
unsafe fn config_from_args(args: *const JavaVMInitArgs) -> VmConfig {
    let mut cfg = VmConfig::with_host_jdk_default();
    if args.is_null() {
        return cfg;
    }
    let init = &*args;
    if init.options.is_null() || init.n_options <= 0 {
        return cfg;
    }
    let opts = std::slice::from_raw_parts(init.options, init.n_options as usize);

    let mut classpath: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < opts.len() {
        let opt = &opts[i];
        if opt.option_string.is_null() {
            i += 1;
            continue;
        }
        let s = match std::ffi::CStr::from_ptr(opt.option_string).to_str() {
            Ok(s) => s,
            Err(_) => {
                i += 1;
                continue;
            }
        };

        if let Some(size) = s.strip_prefix("-Xmx") {
            if let Some(bytes) = parse_mem_size(size) {
                cfg = cfg.with_max_heap_size(bytes);
            }
        } else if s == "-cp" || s == "-classpath" || s == "--class-path" {
            // Path is the following option.
            if let Some(next) = opts.get(i + 1) {
                if !next.option_string.is_null() {
                    if let Ok(p) = std::ffi::CStr::from_ptr(next.option_string).to_str() {
                        push_classpath(&mut classpath, p);
                    }
                }
                i += 2;
                continue;
            }
        } else if let Some(p) = s
            .strip_prefix("-cp=")
            .or_else(|| s.strip_prefix("-classpath="))
            .or_else(|| s.strip_prefix("--class-path="))
        {
            push_classpath(&mut classpath, p);
        } else if let Some(def) = s.strip_prefix("-D") {
            if let Some((k, v)) = def.split_once('=') {
                cfg.system_properties.push((k.to_string(), v.to_string()));
            } else {
                cfg.system_properties.push((def.to_string(), String::new()));
            }
        }
        // else: unrecognised — ignored in increment 1.
        i += 1;
    }

    if !classpath.is_empty() {
        cfg = cfg.with_classpath(classpath);
    }
    cfg
}

/// Split a classpath string on the platform separator and append the parts.
fn push_classpath(out: &mut Vec<String>, raw: &str) {
    let sep = if cfg!(windows) { ';' } else { ':' };
    for part in raw.split(sep) {
        if !part.is_empty() {
            out.push(part.to_string());
        }
    }
}

/// Parse a HotSpot-style memory size (`256m`, `1g`, `1048576`, `512k`) to bytes.
fn parse_mem_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024usize),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1usize),
    };
    num.trim().parse::<usize>().ok().map(|n| n * mult)
}

/// Run the bootstrap init sequence on a freshly-created `Vm`, mirroring the
/// `vm-cli` reference embedder (`vm-cli/src/main.rs`): run `System.initPhase1`
/// when booting real JDK classes (best-effort — it may fall back to synthetic
/// streams), then advance the init level to 4 ("VM fully initialized") so
/// embedder-driven calls see a consistent boot state.
fn bootstrap(vm: &mut Vm) {
    if vm.shared.config.java_home.is_some() {
        // Best-effort: initPhase1 may throw mid-bootstrap on a real JDK; the
        // fallback path (synthetic streams) still leaves the VM usable for
        // static-method invocation, exactly as in vm-cli.
        if vm
            .invoke("java/lang/System", "initPhase1", "()V", &[])
            .is_err()
        {
            vm.main_thread.invoke_cache.clear();
            vm.shared.resolution_cache.write().clear();
        }
        vm.shared.set_init_level(2);
    }
    vm.shared.set_init_level(3);
    vm.shared.set_init_level(4);
}

/// `jint JNI_CreateJavaVM(JavaVM **pvm, void **penv, void *args)`
///
/// The Invocation-API bootstrap entry point. Constructs a [`Vm`] from the
/// supplied [`JavaVMInitArgs`], runs the init phases, parks the VM in the
/// process-global registry, sets the calling thread's JNI TLS context, and
/// writes back the `JavaVM*` (invocation table) and `JNIEnv*` (function table).
///
/// Returns [`JNI_OK`] on success, [`JNI_EEXIST`] if a VM already exists
/// (one-VM-per-process), [`JNI_EINVAL`] on bad pointers, or [`JNI_ERR`] on a
/// caught panic / unexpected failure.
///
/// # Safety
/// `pvm` and `penv` must be writable out-pointers; `args`, when non-null, must
/// be a valid `*const JavaVMInitArgs`.
#[no_mangle]
pub extern "C" fn JNI_CreateJavaVM(
    pvm: *mut JavaVM,
    penv: *mut *mut c_void,
    args: *mut c_void,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if pvm.is_null() || penv.is_null() {
            return JNI_EINVAL;
        }

        // One-VM-per-process: take the registry lock and bail if already set.
        let mut guard = match CREATED_VM.lock() {
            Ok(g) => g,
            Err(_) => return JNI_ERR,
        };
        if guard.is_some() {
            return JNI_EEXIST;
        }

        // SAFETY: `args` is a valid `*const JavaVMInitArgs` or null (handled).
        let config = unsafe { config_from_args(args as *const JavaVMInitArgs) };

        let mut vm = Box::new(Vm::new(config));
        bootstrap(&mut vm);

        // Capture the Arc before moving `vm` into the parked wrapper.
        let shared = vm.shared.get_arc();

        // Publish this thread's JNI context so the returned `JNIEnv*`'s
        // function-table calls (FindClass / GetStaticMethodID / CallStatic…)
        // resolve the live VM via TLS — reusing the exact mechanism native
        // methods use.
        set_jni_context_arc(shared);

        // Hand back the process-global tables from jni.rs.
        // SAFETY: out-pointers checked non-null above.
        unsafe {
            *pvm = get_java_vm();
            *penv = get_jni_env() as *mut c_void;
        }

        // Park the VM for the life of the process.
        *guard = Some(ParkedVm(vm));

        JNI_OK
    }))
    .unwrap_or(JNI_ERR)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_init_args_populates_version() {
        let mut args = JavaVMInitArgs {
            version: 0,
            n_options: 7,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 1,
        };
        let rc = JNI_GetDefaultJavaVMInitArgs(&mut args as *mut _ as *mut c_void);
        assert_eq!(rc, JNI_OK);
        assert_eq!(args.version, JNI_VERSION);
        assert_eq!(args.version, 0x0001_0008);
        // Option fields are reset.
        assert_eq!(args.n_options, 0);
        assert!(args.options.is_null());
    }

    #[test]
    fn default_init_args_null_is_err() {
        assert_eq!(JNI_GetDefaultJavaVMInitArgs(std::ptr::null_mut()), JNI_ERR);
    }

    #[test]
    fn created_vms_reports_count_with_null_buf() {
        // Before any create (in this test process, no create is invoked here to
        // avoid the heavy bootstrap), the count must be 0 and the call must not
        // touch the null buffer.
        let mut n: JSize = -1;
        let rc = JNI_GetCreatedJavaVMs(std::ptr::null_mut(), 0, &mut n as *mut JSize);
        assert_eq!(rc, JNI_OK);
        // count is 0 unless a sibling test in this binary created a VM first;
        // either way it must be a valid non-negative count that was written.
        assert!(n == 0 || n == 1);
    }

    #[test]
    fn mem_size_parsing() {
        assert_eq!(parse_mem_size("256m"), Some(256 * 1024 * 1024));
        assert_eq!(parse_mem_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_mem_size("512k"), Some(512 * 1024));
        assert_eq!(parse_mem_size("1048576"), Some(1_048_576));
        assert_eq!(parse_mem_size(""), None);
        assert_eq!(parse_mem_size("abc"), None);
    }

    #[test]
    fn classpath_split_uses_platform_sep() {
        let mut out = Vec::new();
        if cfg!(windows) {
            push_classpath(&mut out, "a.jar;b.jar;;c.jar");
        } else {
            push_classpath(&mut out, "a.jar:b.jar::c.jar");
        }
        assert_eq!(out, vec!["a.jar", "b.jar", "c.jar"]);
    }
}
