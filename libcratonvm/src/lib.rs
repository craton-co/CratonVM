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

use std::fmt;
use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

// `CompatibilityMode` is reached through `cratonvm_vm::config` on purpose:
// `cratonvm-types` (where the type is actually defined, as
// `cratonvm_types::compat::CompatibilityMode`) is only a **dev-dependency** of
// this crate, so naming it directly would compile the tests and break the
// cdylib. The re-export in `vm/src/config.rs` is the supported path; promoting
// the dependency is a separate decision about what the shipped .so links.
use cratonvm_vm::config::{CompatibilityMode, VmConfig};
use cratonvm_vm::native::jni::{
    clear_jni_context, get_java_vm, get_jni_env, host_thread_enter_native,
    host_thread_leave_native, set_destroy_vm_hook, set_jni_context_arc,
};
use cratonvm_vm::vm::Vm;

// ---------------------------------------------------------------------------
// JNI integer / handle types (re-exported from the vm crate so the C ABI is
// identical to the function-table side).
// ---------------------------------------------------------------------------

pub type JInt = i32;
pub type JNIEnv = *const *const usize;
pub type JSize = i32;
pub type JavaVM = *const *const usize;

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

pub const CRATONVM_JNI_OK: JInt = JNI_OK;
pub const CRATONVM_JNI_ERR: JInt = JNI_ERR;
pub const CRATONVM_JNI_EDETACHED: JInt = JNI_EDETACHED;
pub const CRATONVM_JNI_EVERSION: JInt = JNI_EVERSION;
pub const CRATONVM_JNI_ENOMEM: JInt = JNI_ENOMEM;
pub const CRATONVM_JNI_EEXIST: JInt = JNI_EEXIST;
pub const CRATONVM_JNI_EINVAL: JInt = JNI_EINVAL;
pub const CRATONVM_JNI_VERSION: JInt = JNI_VERSION;

const JNI_VERSION_1_1: JInt = 0x0001_0001;
const JNI_VERSION_1_2: JInt = 0x0001_0002;
const JNI_VERSION_1_4: JInt = 0x0001_0004;
const JNI_VERSION_1_6: JInt = 0x0001_0006;

fn is_supported_jni_version(version: JInt) -> bool {
    matches!(
        version,
        JNI_VERSION_1_1 | JNI_VERSION_1_2 | JNI_VERSION_1_4 | JNI_VERSION_1_6 | JNI_VERSION
    )
}

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
    CREATED_VM.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// The flat API and Invocation API both feed the VM crate's process-global JNI
/// context. Until that lower layer has a per-handle JavaVM back-pointer, this
/// crate must fail closed and allow only one active VM surface in a process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlatVmState {
    Creating,
    Active(usize),
}

static FLAT_VM_STATE: Mutex<Option<FlatVmState>> = Mutex::new(None);

struct FlatVmCreateGuard {
    armed: bool,
}

fn begin_flat_vm_create() -> Result<FlatVmCreateGuard, &'static str> {
    let mut state = FLAT_VM_STATE
        .lock()
        .map_err(|_| "flat VM state is poisoned")?;
    if state.is_some() {
        return Err("a CratonVM instance is already active in this process");
    }
    *state = Some(FlatVmState::Creating);
    Ok(FlatVmCreateGuard { armed: true })
}

impl FlatVmCreateGuard {
    fn activate(&mut self, vm_key: usize) -> Result<(), &'static str> {
        let mut state = FLAT_VM_STATE
            .lock()
            .map_err(|_| "flat VM state is poisoned")?;
        if *state != Some(FlatVmState::Creating) {
            return Err("flat VM state changed during creation");
        }
        *state = Some(FlatVmState::Active(vm_key));
        self.armed = false;
        Ok(())
    }
}

impl Drop for FlatVmCreateGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = FLAT_VM_STATE.lock() {
            if *state == Some(FlatVmState::Creating) {
                *state = None;
            }
        }
    }
}

fn release_flat_vm(vm_key: usize) {
    if let Ok(mut state) = FLAT_VM_STATE.lock() {
        if *state == Some(FlatVmState::Active(vm_key)) {
            *state = None;
        }
    }
}

fn flat_vm_blocks_invocation_create() -> bool {
    FLAT_VM_STATE
        .lock()
        .map(|state| state.is_some())
        .unwrap_or(true)
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
        // writes back the supported version. We also accept version 0 as a
        // compatibility "discover the default" request used by older examples.
        let requested_version = init.version;
        init.version = JNI_VERSION;
        init.n_options = 0;
        init.options = std::ptr::null_mut();
        init.ignore_unrecognized = 0;
        if requested_version != 0 && !is_supported_jni_version(requested_version) {
            JNI_EVERSION
        } else {
            JNI_OK
        }
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

// Errors while validating / parsing JavaVMInitArgs. JNI_CreateJavaVM maps
// UnsupportedVersion to JNI_EVERSION and all option/shape errors to JNI_ERR.
#[derive(Clone, Debug, Eq, PartialEq)]
enum InitArgsError {
    UnsupportedVersion(JInt),
    InvalidArgs(&'static str),
    InvalidOption(String),
    UnrecognizedOption(String),
    /// The selected JDK mode has no backing class library in this
    /// build/environment. Carries the full, actionable message produced by
    /// `require_real_jdk` / `require_synthetic_jdk`.
    JdkModeUnavailable(String),
    /// The `compatibility_mode` argument of
    /// [`cratonvm_create_with_compatibility`] is not a
    /// `CRATONVM_COMPATIBILITY_*` value. Carries the offending integer.
    ///
    /// A distinct variant rather than an `InvalidOption`: the fault is in a
    /// typed C argument, not in a `JavaVMInitArgs` option string, and
    /// `ignoreUnrecognized` must not soften it.
    UnknownCompatibilityMode(JInt),
    /// [`VmConfig::validate_compatibility`] rejected the assembled config.
    /// Carries that [`cratonvm_vm::error::VmError::InvalidConfiguration`]'s
    /// `Display` text **verbatim**, so the rule is called, not re-derived
    /// here — see [`finish_config`].
    InvalidCompatibility(String),
}

impl fmt::Display for InitArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitArgsError::UnsupportedVersion(version) => {
                write!(f, "unsupported JNI version 0x{version:08x}")
            }
            InitArgsError::InvalidArgs(msg) => f.write_str(msg),
            InitArgsError::InvalidOption(msg) => f.write_str(msg),
            InitArgsError::UnrecognizedOption(opt) => {
                write!(f, "unrecognized JavaVM option {opt:?}")
            }
            InitArgsError::JdkModeUnavailable(msg) => f.write_str(msg),
            InitArgsError::UnknownCompatibilityMode(value) => write!(
                f,
                "unknown compatibility mode {value}: expected {} (compatible) or {} (jdk-only)",
                craton_compatibility::COMPATIBLE,
                craton_compatibility::JDK_ONLY
            ),
            // Verbatim: byte-for-byte the `VmError::InvalidConfiguration` text,
            // so the C ABI and the launcher report the same conflict in the
            // same words. A test pins the equality.
            InitArgsError::InvalidCompatibility(msg) => f.write_str(msg),
        }
    }
}

fn validate_or_ignore(
    ignore_unrecognized: bool,
    error: InitArgsError,
) -> Result<(), InitArgsError> {
    if ignore_unrecognized {
        Ok(())
    } else {
        Err(error)
    }
}

unsafe fn option_to_str<'a>(
    opts: &'a [JavaVMOption],
    index: usize,
    ignore_unrecognized: bool,
) -> Result<Option<&'a str>, InitArgsError> {
    let opt = &opts[index];
    if opt.option_string.is_null() {
        validate_or_ignore(
            ignore_unrecognized,
            InitArgsError::InvalidOption(format!("JavaVM option {index} has a null optionString")),
        )?;
        return Ok(None);
    }
    match std::ffi::CStr::from_ptr(opt.option_string).to_str() {
        Ok(s) => Ok(Some(s)),
        Err(_) => {
            validate_or_ignore(
                ignore_unrecognized,
                InitArgsError::InvalidOption(format!("JavaVM option {index} is not valid UTF-8")),
            )?;
            Ok(None)
        }
    }
}

/// Validate that the class library the config selected actually exists, and
/// pin the resolved `JAVA_HOME` onto the config when it does.
///
/// **This is the C-ABI equivalent of `vm-cli`'s `resolve_jdk_mode`.** Without
/// it, `JNI_CreateJavaVM` / `cratonvm_create` could start in a mode whose
/// backing library is not there:
///
/// * real-JDK mode with no usable JDK on the host boots with an empty boot
///   classpath, surfacing much later as an unexplained `NoClassDefFoundError`
///   on the first JDK class reference;
/// * synthetic mode in a build without the `synthetic-jdk` Cargo feature
///   registers none of the ~5,200 stubs **and** suppresses boot-classpath
///   discovery, i.e. a VM with no class library at all.
///
/// Both are hard errors at the launcher; they must be hard errors here too.
/// See `jdk-mode-determinism.md` §2.3 and §6.4.
///
/// The resolved root is written back to `java_home` (when not already set) for
/// the same reason the launcher does it: so the VM's boot-classpath discovery
/// resolves the installation that was just validated instead of re-running the
/// environment probe and possibly landing somewhere else.
fn validate_jdk_mode(cfg: VmConfig) -> Result<VmConfig, InitArgsError> {
    use cratonvm_vm::config as vmcfg;
    match cfg.jdk_mode() {
        vmcfg::JdkMode::Synthetic => {
            vmcfg::require_synthetic_jdk().map_err(InitArgsError::JdkModeUnavailable)?;
            Ok(cfg)
        }
        vmcfg::JdkMode::Real => {
            let home = vmcfg::require_real_jdk(cfg.java_home.as_deref())
                .map_err(InitArgsError::JdkModeUnavailable)?;
            if cfg.java_home.is_none() {
                Ok(cfg.with_java_home(home.to_string_lossy().into_owned()))
            } else {
                Ok(cfg)
            }
        }
    }
}

/// Resolve the compatibility mode from the two explicit sources — the typed
/// `compatibility_mode` argument (`requested`) and the `--jdk-only` option
/// string — and record it on the config.
///
/// # The four rules, each deliberate
///
/// * **Neither stated → [`DEFAULT_COMPATIBILITY_MODE`]**
///   ([`CompatibilityMode::Compatible`]). The default is reached by doing
///   nothing; see that constant for why it is never inferred from anything else.
/// * **`--jdk-only` alone → [`CompatibilityMode::JdkOnly`].** It is spelled
///   exactly as the launcher flag, and it is the *only* route to strict mode
///   from [`JNI_CreateJavaVM`], which takes nothing but a [`JavaVMInitArgs`].
/// * **[`CRATONVM_COMPATIBILITY_JDK_ONLY`] plus `--jdk-only` → strict.** They
///   agree; agreeing twice is not an error.
/// * **[`CRATONVM_COMPATIBILITY_COMPATIBLE`] plus `--jdk-only` → a
///   contradiction error, not a precedence rule.** Both are explicit statements
///   of policy. Picking a winner would mean a host that assembled its options
///   from two places (a config file and a call site, say) runs under a policy
///   neither half asked for — silently strict, or silently loose. Either is
///   worse than being told.
///
/// # This does not rewrite the JDK mode
///
/// `--jdk-only` implies a real JDK image, and the base config here is already
/// [`VmConfig::with_host_jdk_default`] (real), so strict-on-its-own lands on
/// real + strict with nothing to do. Forcing `JdkMode::Real` anyway would
/// silently repair `--jdk-only --synthetic-jdk` into a real-JDK run and erase
/// the conflict [`finish_config`] exists to report.
fn apply_compatibility(
    cfg: VmConfig,
    requested: Option<CompatibilityMode>,
    jdk_only_option: bool,
) -> Result<VmConfig, InitArgsError> {
    let mode = match (requested, jdk_only_option) {
        (None, false) => DEFAULT_COMPATIBILITY_MODE,
        (None, true) => CompatibilityMode::JdkOnly,
        (Some(CompatibilityMode::JdkOnly), _) => CompatibilityMode::JdkOnly,
        (Some(CompatibilityMode::Compatible), false) => CompatibilityMode::Compatible,
        (Some(CompatibilityMode::Compatible), true) => {
            return Err(InitArgsError::InvalidOption(format!(
                "compatibility mode {} (compatible) was requested together with the \
                 --jdk-only option, which requests {} (jdk-only): both are explicit \
                 statements of compatibility policy and they contradict. Pass \
                 CRATONVM_COMPATIBILITY_JDK_ONLY alongside --jdk-only, or drop one of \
                 the two.",
                craton_compatibility::COMPATIBLE,
                craton_compatibility::JDK_ONLY
            )));
        }
    };
    Ok(cfg.with_compatibility_mode(mode))
}

/// Final validation of an assembled config: **compatibility coherence first,
/// then class-library availability.**
///
/// The order is normative, not incidental. `--jdk-only` with `--synthetic-jdk`
/// is wrong on a machine with a perfect JDK installation and equally wrong on
/// one with none, so the verdict must be a property of the *request* rather
/// than of what happens to be installed. Running [`validate_jdk_mode`] first
/// would report "no usable JDK was found" for a request that was incoherent
/// before the host was ever consulted, and the operator would go install a JDK
/// to fix a flag conflict.
///
/// The [`cratonvm_vm::error::VmError::InvalidConfiguration`] message is carried
/// through **unchanged** in [`InitArgsError::InvalidCompatibility`]: the rule
/// lives in [`VmConfig::validate_compatibility`] and is *called* here, never
/// re-derived, so the C ABI and the `cratonvm` launcher cannot drift apart.
fn finish_config(cfg: VmConfig) -> Result<VmConfig, InitArgsError> {
    cfg.validate_compatibility()
        .map_err(|e| InitArgsError::InvalidCompatibility(e.to_string()))?;
    validate_jdk_mode(cfg)
}

/// Parse a `JavaVMInitArgs` into a [`VmConfig`], with the compatibility mode
/// left implicit — i.e. taken from the `--jdk-only` option string if present,
/// and [`DEFAULT_COMPATIBILITY_MODE`] otherwise.
///
/// This is the shape [`JNI_CreateJavaVM`] and [`cratonvm_create`] need: neither
/// has anywhere to put a typed policy argument. See
/// [`config_from_args_with_compatibility`] for the full contract.
///
/// # Safety
/// `args` must be a valid `*const JavaVMInitArgs` with `options` pointing at
/// `n_options` valid, NUL-terminated [`JavaVMOption`] strings.
unsafe fn config_from_args(args: *const JavaVMInitArgs) -> Result<VmConfig, InitArgsError> {
    config_from_args_with_compatibility(args, None)
}

/// Parse a `JavaVMInitArgs` into a [`VmConfig`].
///
/// Recognises the common HotSpot option forms an embedder is likely to pass:
/// `-Xmx<size>`, `-cp`/`-classpath`/`--class-path <path>` (the path may be the
/// next option or glued as `-cp=<path>`), and `-D<key>=<value>`. Unrecognised
/// options are ignored only when `ignoreUnrecognized` is non-zero.
///
/// # JDK mode
///
/// The base config is [`VmConfig::for_launcher`] (via the retained
/// `with_host_jdk_default` alias): deterministic real-JDK mode, never
/// host-detected. `--real-jdk` / `--synthetic-jdk` are accepted as JavaVM
/// options with exactly the launcher's spelling and exactly the launcher's
/// mutual-exclusion rule, so an embedder is not silently locked out of the
/// synthetic library that host autodetection used to hand it on a JDK-less
/// machine. Whichever mode results is then validated by [`validate_jdk_mode`]
/// — this entry point performed no validation at all before.
///
/// # Compatibility mode
///
/// `requested` is the typed policy argument of
/// [`cratonvm_create_with_compatibility`] (`None` for the entry points that
/// have no such argument). The `--jdk-only` option string is the second route,
/// spelled exactly as the launcher flag so a command line pasted into a
/// `JavaVMInitArgs` behaves identically — and the only route available to
/// [`JNI_CreateJavaVM`]. [`apply_compatibility`] resolves the two; note that a
/// `--jdk-only` option is a *recognised* flag, so `ignoreUnrecognized` neither
/// drops it nor softens its contradiction with an explicit
/// [`CRATONVM_COMPATIBILITY_COMPATIBLE`], exactly as for
/// `--real-jdk`/`--synthetic-jdk`.
///
/// # Safety
/// `args` must be a valid `*const JavaVMInitArgs` with `options` pointing at
/// `n_options` valid, NUL-terminated [`JavaVMOption`] strings.
unsafe fn config_from_args_with_compatibility(
    args: *const JavaVMInitArgs,
    requested: Option<CompatibilityMode>,
) -> Result<VmConfig, InitArgsError> {
    let mut cfg = VmConfig::with_host_jdk_default();
    if args.is_null() {
        return finish_config(apply_compatibility(cfg, requested, false)?);
    }
    let init = &*args;
    if !is_supported_jni_version(init.version) {
        return Err(InitArgsError::UnsupportedVersion(init.version));
    }
    if init.n_options < 0 {
        return Err(InitArgsError::InvalidArgs(
            "JavaVMInitArgs.nOptions must not be negative",
        ));
    }
    if init.n_options == 0 {
        return finish_config(apply_compatibility(cfg, requested, false)?);
    }
    if init.options.is_null() {
        return Err(InitArgsError::InvalidArgs(
            "JavaVMInitArgs.options is null but nOptions is nonzero",
        ));
    }
    let opts = std::slice::from_raw_parts(init.options, init.n_options as usize);
    let ignore_unrecognized = init.ignore_unrecognized != 0;

    let mut classpath: Vec<String> = Vec::new();
    // Same flags, same spelling, same mutual exclusion as the launcher.
    let mut synthetic_flag = false;
    let mut real_flag = false;
    // Compatibility policy, spelled exactly as the launcher's `--jdk-only`.
    let mut jdk_only_flag = false;
    let mut i = 0usize;
    while i < opts.len() {
        let Some(s) = option_to_str(opts, i, ignore_unrecognized)? else {
            i += 1;
            continue;
        };

        if let Some(size) = s.strip_prefix("-Xmx") {
            if let Some(bytes) = parse_mem_size(size) {
                cfg = cfg.with_max_heap_size(bytes);
            } else {
                validate_or_ignore(
                    ignore_unrecognized,
                    InitArgsError::InvalidOption(format!("invalid -Xmx size in option {s:?}")),
                )?;
            }
        } else if s == "-cp" || s == "-classpath" || s == "--class-path" {
            // Path is the following option.
            if let Some(next) = opts.get(i + 1) {
                if next.option_string.is_null() {
                    validate_or_ignore(
                        ignore_unrecognized,
                        InitArgsError::InvalidOption(format!(
                            "classpath option {s:?} has a null value"
                        )),
                    )?;
                } else {
                    match std::ffi::CStr::from_ptr(next.option_string).to_str() {
                        Ok(p) => push_classpath(&mut classpath, p),
                        Err(_) => validate_or_ignore(
                            ignore_unrecognized,
                            InitArgsError::InvalidOption(format!(
                                "classpath value after option {s:?} is not valid UTF-8"
                            )),
                        )?,
                    }
                }
                i += 2;
                continue;
            } else {
                validate_or_ignore(
                    ignore_unrecognized,
                    InitArgsError::InvalidOption(format!(
                        "classpath option {s:?} requires a following value"
                    )),
                )?;
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
        } else if s == "--synthetic-jdk" {
            synthetic_flag = true;
        } else if s == "--real-jdk" {
            real_flag = true;
        } else if s == "--jdk-only" {
            // NOT `cfg.with_jdk_mode(Real)`: see `apply_compatibility`. The
            // base config is already real, and rewriting it here would erase
            // the `--synthetic-jdk` conflict `finish_config` reports.
            jdk_only_flag = true;
        } else if matches!(s, "vfprintf" | "exit" | "abort") {
            // Standard Invocation API callbacks are recognized but unused.
        } else {
            validate_or_ignore(
                ignore_unrecognized,
                InitArgsError::UnrecognizedOption(s.to_string()),
            )?;
        }
        i += 1;
    }

    if !classpath.is_empty() {
        cfg = cfg.with_classpath(classpath);
    }

    // Mode selection. Passing both is an error rather than a silent
    // last-one-wins: they select two different standard-library
    // implementations, so guessing is never the right answer. `ignoreUnrecognized`
    // does NOT soften this — the flags were recognized, they just contradict.
    if synthetic_flag && real_flag {
        return Err(InitArgsError::InvalidOption(format!(
            "--synthetic-jdk and --real-jdk are mutually exclusive: they select two \
             different standard-library implementations. Pass exactly one (or neither, \
             for the default {}).",
            cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE
        )));
    }
    if synthetic_flag {
        cfg = cfg.with_jdk_mode(cratonvm_vm::config::JdkMode::Synthetic);
    } else if real_flag {
        cfg = cfg.with_jdk_mode(cratonvm_vm::config::JdkMode::Real);
    }

    finish_config(apply_compatibility(cfg, requested, jdk_only_flag)?)
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
            vm.shared.classes.resolution_cache.write().clear();
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
/// (one-VM-per-process), [`JNI_EVERSION`] for an unsupported requested JNI
/// version, [`JNI_EINVAL`] on bad pointers, or [`JNI_ERR`] on invalid options,
/// a caught panic, or an unexpected failure.
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
        if flat_vm_blocks_invocation_create() {
            return JNI_EEXIST;
        }

        // SAFETY: `args` is a valid `*const JavaVMInitArgs` or null (handled).
        let config = match unsafe { config_from_args(args as *const JavaVMInitArgs) } {
            Ok(config) => config,
            Err(InitArgsError::UnsupportedVersion(_)) => return JNI_EVERSION,
            Err(_) => return JNI_ERR,
        };

        let mut vm = Box::new(Vm::new(config));
        bootstrap(&mut vm);

        // Capture the Arc before moving `vm` into the parked wrapper.
        // (`Vm::new` already published the process-global VM cell that
        // `AttachCurrentThread` resolves — see `jni::set_process_vm`.)
        let shared = vm.shared.get_arc();

        // Publish this thread's JNI context so the returned `JNIEnv*`'s
        // function-table calls (FindClass / GetStaticMethodID / CallStatic…)
        // resolve the live VM via TLS — reusing the exact mechanism native
        // methods use.
        set_jni_context_arc(shared);

        // Register the teardown the `DestroyJavaVM` invocation-table slot
        // invokes: this crate owns the parked VM, so the vm crate's slot
        // delegates back here to drop it. See `destroy_created_vm`.
        set_destroy_vm_hook(destroy_created_vm);

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

/// Tear down the Invocation-API VM: the teardown registered with
/// [`set_destroy_vm_hook`] and invoked by the vm crate's `DestroyJavaVM` slot.
///
/// Takes the parked VM out of [`CREATED_VM`] and drops it — releasing its
/// `Arc<SharedVm>`, heap, and threads — then clears the calling thread's JNI TLS
/// context (so a later JNIEnv-table call on this thread does not resolve a freed
/// VM) and drops this VM's flat-API handle table (releasing the global refs that
/// pinned its objects as GC roots, in case the host mixed the two surfaces).
/// After this, [`JNI_GetCreatedJavaVMs`] reports 0.
///
/// One-VM-per-process / restart caveat: HotSpot does not support recreating a VM
/// after `DestroyJavaVM`, and neither do we — CratonVM installs process-global
/// signal handlers / sandbox roots and leaks the JNI function tables as
/// process-lifetime singletons (see the design doc Risks). Clearing the registry
/// makes the count honest and frees the VM instance; a *subsequent*
/// `JNI_CreateJavaVM` in the same process is untested and unsupported.
fn destroy_created_vm() -> JInt {
    // SAFETY: pure Rust; wrapped in catch_unwind so a drop panic cannot unwind
    // across the C `DestroyJavaVM` boundary.
    catch_unwind(AssertUnwindSafe(|| {
        let parked = match CREATED_VM.lock() {
            Ok(mut g) => g.take(),
            Err(_) => return JNI_ERR,
        };
        if let Some(parked) = parked {
            // Release this VM's flat-handle table + global refs before dropping
            // the VM (the SharedVm address keys the table; resolve it first).
            drop_handle_table(&parked.0.shared);
            // Clear the JNI TLS context for the calling thread: it was published
            // to point at this VM; once dropped that Arc would dangle in TLS.
            clear_jni_context();
            drop(parked);
        }
        JNI_OK
    }))
    .unwrap_or(JNI_ERR)
}

// ---------------------------------------------------------------------------
// Host thread in-native transition
// ---------------------------------------------------------------------------

/// `jint cratonvm_thread_enter_native(void)`
///
/// Declare the **calling** OS thread as parked in host-native code (HotSpot's
/// `_thread_in_native`): it is excluded from GC stop-the-world for the duration,
/// so a collection driven by another thread does not wait for it to reach a Java
/// safepoint it will never hit while parked outside the VM.
///
/// Call this around any host-side blocking wait (a `join()`, an event-loop poll,
/// `sleep`) on a thread that drives the VM but is currently idle in native code —
/// most importantly the **creating thread** after `JNI_CreateJavaVM`, while other
/// (attached) threads run Java + GC. Without it, that idle thread is counted as a
/// live mutator and hangs the collection. Balance every call with exactly one
/// [`cratonvm_thread_leave_native`].
///
/// A **foreign attached** thread (`AttachCurrentThread`) does not need this — it
/// is already modelled as in-native between its JNI calls — so the call is a
/// no-op for it. Returns [`JNI_OK`], or [`JNI_ERR`] if no VM exists.
#[no_mangle]
pub extern "C" fn cratonvm_thread_enter_native() -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if host_thread_enter_native() {
            JNI_OK
        } else {
            JNI_ERR
        }
    }))
    .unwrap_or(JNI_ERR)
}

/// `jint cratonvm_thread_leave_native(void)`
///
/// Re-enter the VM after [`cratonvm_thread_enter_native`]: the calling thread
/// rejoins the mutator population, waiting out any in-flight stop-the-world
/// first. Balance exactly one prior `cratonvm_thread_enter_native`. No-op for a
/// foreign attached thread. Returns [`JNI_OK`], or [`JNI_ERR`] if no VM exists.
#[no_mangle]
pub extern "C" fn cratonvm_thread_leave_native() -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if host_thread_leave_native() {
            JNI_OK
        } else {
            JNI_ERR
        }
    }))
    .unwrap_or(JNI_ERR)
}

// ===========================================================================
// Layer 2 — flat opaque-handle C API (`cratonvm_*`)
// ===========================================================================
//
// This is **Layer 2** of `docs/feature-designs/embedding-api.md`: a flat C API
// of `extern "C"` functions over **opaque handles + POD** that wraps the
// Layer-1 Rust `Vm` directly. No Rust types cross the boundary.
//
// ## Relationship to the Layer-1 / Invocation-API side
//
// The `JNI_CreateJavaVM` path above is the `libjvm`-substitute bootstrap: it
// publishes a *process-global* `JavaVM*`/`JNIEnv*` and parks the single VM in
// `CREATED_VM`. The flat API here is the *curated convenience* surface for a
// host that does not want to drive the raw 234-slot JNIEnv table by index. It
// gives the caller an explicit owned handle (`*mut CratonVm`) whose lifetime
// the caller controls via `cratonvm_destroy`.
//
// Both surfaces sit on the same lower-level JNI machinery: `Vm::new` publishes
// a process-global `Weak<SharedVm>`, and the JNIEnv table resolves the live VM
// through thread-local context. Those cells do not have a per-handle back
// pointer, so libcratonvm deliberately allows only one active VM surface per
// process: either one Invocation-API VM or one flat `CratonVm`. Each flat API
// entry point republishes its handle's JNI TLS context before entering the VM,
// so stale TLS from a prior flat call cannot point at a different handle. (The
// GC-safepoint thread-registration contract for *foreign* call-in threads is
// owned by another work item and is intentionally out of scope here; the flat
// API only drives the VM from the creating thread.)
//
// ## Handle encoding
//
// * `CratonVm*` — owning, opaque; `Box<Vm>` behind the pointer.
// * `CratonClass` (`u64`) — a `ClassId` widened from its `u32`; `0` is a valid
//   class id (`java/lang/Object` is id 0), so class errors are signalled by
//   the function's return code, not a sentinel handle.
// * `CratonRef` (`u64`) - an opaque object/string token validated against a
//   per-VM table; `0` == `null`.
//
// ## Value marshalling
//
// `CratonValue` is a `#[repr(C)]` tagged POD (tag + 8-byte payload) mirroring
// the subset of `cratonvm_vm`'s `Value` an embedder exchanges. Method args are
// passed as a `*const CratonValue` + count (not C varargs: varargs across FFI
// are unsound for non-`int`/`double` types and not ABI-portable — a typed
// array is the stable form; a varargs shim is noted as a next step).

use std::cell::RefCell;
use std::ffi::{CStr, CString};

use cratonvm_vm::types::{ObjectRef, Value};
use cratonvm_vm::ClassId;
use cratonvm_vm::MethodCallFailed;

/// Opaque VM handle returned by [`cratonvm_create`]. The host treats this as a
/// pointer-sized token; the only valid operations are passing it back to the
/// other `cratonvm_*` functions and finally to [`cratonvm_destroy`].
pub struct CratonVm {
    vm: Vm,
}

/// A `u64` opaque object/string/throwable token (`0` = null).
pub type CratonRef = u64;
/// A `u64` class handle (a widened [`ClassId`]).
pub type CratonClass = u64;

/// Discriminant for [`CratonValue`].
pub mod craton_tag {
    use super::JInt;
    /// No value (void return) / empty slot.
    pub const VOID: JInt = 0;
    /// 32-bit int (also boolean/byte/char/short).
    pub const INT: JInt = 1;
    /// 64-bit long.
    pub const LONG: JInt = 2;
    /// 32-bit float (bit-cast into the low 32 bits of the payload).
    pub const FLOAT: JInt = 3;
    /// 64-bit double (bit-cast into the payload).
    pub const DOUBLE: JInt = 4;
    /// Object/string reference handle ([`super::CratonRef`]).
    pub const OBJECT: JInt = 5;
    /// The call failed; inspect [`super::cratonvm_last_error`].
    pub const ERROR: JInt = -1;
}

pub const CRATON_TAG_VOID: JInt = craton_tag::VOID;
pub const CRATON_TAG_INT: JInt = craton_tag::INT;
pub const CRATON_TAG_LONG: JInt = craton_tag::LONG;
pub const CRATON_TAG_FLOAT: JInt = craton_tag::FLOAT;
pub const CRATON_TAG_DOUBLE: JInt = craton_tag::DOUBLE;
pub const CRATON_TAG_OBJECT: JInt = craton_tag::OBJECT;
pub const CRATON_TAG_ERROR: JInt = craton_tag::ERROR;

/// ABI encoding of [`CompatibilityMode`] — *which substitutions the VM may
/// make*, orthogonal to the JDK mode (*which class library it boots*).
///
/// These numeric values are a **published, stable, append-only** part of the C
/// ABI: a compiled host carries them in its `.text`, so a value may be added
/// but never renumbered or re-typed. They are `cratonvm_jint`, deliberately not
/// a `cratonvm_jboolean`, so a third enforcement posture can be added later
/// without breaking a host that was compiled against this header.
///
/// See `docs/EMBEDDING.md` ("Choosing a compatibility mode") and
/// `docs/feature-designs/jdk-only-mode.md` for the semantics.
pub mod craton_compatibility {
    use super::JInt;
    /// Today's behaviour: bridges, intrinsics **and** compatibility shims.
    /// The default on every entry point.
    pub const COMPATIBLE: JInt = 0;
    /// Real JDK class bytes are authoritative: no class fabricated without real
    /// bytes, no synthetic-stub native registered or invoked. Requires a real
    /// JDK runtime image.
    pub const JDK_ONLY: JInt = 1;
}

pub const CRATONVM_COMPATIBILITY_COMPATIBLE: JInt = craton_compatibility::COMPATIBLE;
pub const CRATONVM_COMPATIBILITY_JDK_ONLY: JInt = craton_compatibility::JDK_ONLY;

/// The compatibility mode a VM created through this crate runs under when the
/// host asks for nothing: [`CompatibilityMode::Compatible`].
///
/// Aliased to `LAUNCHER_DEFAULT_COMPATIBILITY_MODE` rather than
/// `EMBEDDED_DEFAULT_COMPATIBILITY_MODE` because this crate's base config is
/// [`VmConfig::with_host_jdk_default`] — i.e. the launcher's, not
/// `VmConfig::default`'s. The two constants happen to be equal, and the alias
/// still names the right one so a future divergence lands here correctly.
///
/// # The asymmetry, stated on purpose
///
/// `JdkMode` differs between entry points (`VmConfig::default` is synthetic,
/// `with_host_jdk_default` is real) because *which class library loads* is a
/// hermeticity question and the two entry points genuinely want different
/// answers. **Compatibility mode deliberately does not differ.**
/// [`CompatibilityMode::JdkOnly`] rejects work that
/// [`CompatibilityMode::Compatible`] accepts, so a host that has not asked for
/// it must never be handed it.
///
/// Strictness therefore has exactly two sources here, both explicit: the
/// [`CRATONVM_COMPATIBILITY_JDK_ONLY`] argument to
/// [`cratonvm_create_with_compatibility`], or the `--jdk-only` option string in
/// the `JavaVMInitArgs`. It is never inferred from a Cargo feature, never read
/// from `CRATONVM_REAL` / `CRATONVM_NO_STUBS` (those are native-registry
/// filters and cannot express the class-loading or dispatch half of the
/// contract), and never derived from what the host machine has installed.
pub const DEFAULT_COMPATIBILITY_MODE: CompatibilityMode =
    cratonvm_vm::config::LAUNCHER_DEFAULT_COMPATIBILITY_MODE;

/// Decode a `CRATONVM_COMPATIBILITY_*` ABI integer.
///
/// An unrecognised value is **rejected**, never clamped to
/// [`CompatibilityMode::Compatible`]: a host compiled against a newer header
/// and run against an older library is told so, rather than silently getting
/// the loose policy it believed it had opted out of.
fn compatibility_mode_from_abi(value: JInt) -> Result<CompatibilityMode, InitArgsError> {
    match value {
        craton_compatibility::COMPATIBLE => Ok(CompatibilityMode::Compatible),
        craton_compatibility::JDK_ONLY => Ok(CompatibilityMode::JdkOnly),
        other => Err(InitArgsError::UnknownCompatibilityMode(other)),
    }
}

/// Encode a [`CompatibilityMode`] as its `CRATONVM_COMPATIBILITY_*` ABI
/// integer. Total by construction: adding a mode here is the same commit that
/// appends its constant to [`craton_compatibility`].
fn compatibility_mode_to_abi(mode: CompatibilityMode) -> JInt {
    match mode {
        CompatibilityMode::Compatible => craton_compatibility::COMPATIBLE,
        CompatibilityMode::JdkOnly => craton_compatibility::JDK_ONLY,
    }
}

/// A C-ABI tagged value exchanged with the flat API. `tag` is one of the
/// [`craton_tag`] constants; `payload` is interpreted accordingly:
/// `INT`→low 32 bits (sign-extended), `LONG`→all 64 bits, `FLOAT`→`f32` bits
/// in the low 32, `DOUBLE`→`f64` bits, `OBJECT`→a [`CratonRef`]. For `VOID`
/// and `ERROR` the payload is unspecified (`0`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CratonValue {
    /// One of the [`craton_tag`] discriminants.
    pub tag: JInt,
    /// Bit-packed payload; see the type-level docs.
    pub payload: u64,
}

impl CratonValue {
    const fn void() -> Self {
        CratonValue {
            tag: craton_tag::VOID,
            payload: 0,
        }
    }
    const fn error() -> Self {
        CratonValue {
            tag: craton_tag::ERROR,
            payload: 0,
        }
    }

    /// Convert an inbound `CratonValue` (from C) into a VM [`Value`].
    /// Unknown tags and stale object tokens are rejected rather than silently
    /// becoming `null`; callers turn the error into the API's last-error value.
    fn to_value(self, shared: &SharedVm) -> Result<Value, String> {
        match self.tag {
            craton_tag::INT => Ok(Value::Int(self.payload as u32 as i32)),
            craton_tag::LONG => Ok(Value::Long(self.payload as i64)),
            craton_tag::FLOAT => Ok(Value::Float(f32::from_bits(self.payload as u32))),
            craton_tag::DOUBLE => Ok(Value::Double(f64::from_bits(self.payload))),
            craton_tag::OBJECT if self.payload == 0 => Ok(Value::Object(None)),
            craton_tag::OBJECT => resolve_handle(shared, self.payload)
                .map(|obj| Value::Object(Some(obj)))
                .ok_or_else(|| {
                    format!(
                        "stale or unknown CratonRef object token 0x{:x}",
                        self.payload
                    )
                }),
            craton_tag::VOID => Err("VOID CratonValue is not valid as input".to_string()),
            craton_tag::ERROR => Err("ERROR CratonValue is not valid as input".to_string()),
            other => Err(format!("unknown CratonValue tag {other}")),
        }
    }

    /// Convert an outbound VM [`Value`] into a `CratonValue` for C. An object
    /// reference is registered in `shared`'s per-VM handle table and returned
    /// as an opaque token (never a raw heap address).
    fn from_value(shared: &SharedVm, v: Value) -> Self {
        match v {
            Value::Int(i) => CratonValue {
                tag: craton_tag::INT,
                payload: i as u32 as u64,
            },
            Value::Long(l) => CratonValue {
                tag: craton_tag::LONG,
                payload: l as u64,
            },
            Value::Float(f) => CratonValue {
                tag: craton_tag::FLOAT,
                payload: f.to_bits() as u64,
            },
            Value::Double(d) => CratonValue {
                tag: craton_tag::DOUBLE,
                payload: d.to_bits(),
            },
            Value::Object(o) => CratonValue {
                tag: craton_tag::OBJECT,
                payload: register_handle(shared, o),
            },
            // ReturnAddress / Uninitialized never escape a normal return; treat
            // as void so the C side sees a defined (if empty) result.
            Value::ReturnAddress(_) | Value::Uninitialized => CratonValue::void(),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-VM opaque-handle table.
//
// A `CratonRef` handed to the host MUST NOT be a raw heap address: a host that
// fabricated, retained-past-GC, or corrupted such a value would steer the VM
// into dereferencing an arbitrary pointer (use-after-free / out-of-bounds). So
// every object handle is an **opaque token** validated against a per-VM table
// before it is ever turned back into an `ObjectRef`.
//
// The table is layered on top of the VM's existing `JniGlobalRefs`
// (`shared.natives.jni_global_refs`), which is the only channel that (a) keeps the
// referenced object alive as a GC root and (b) has its stored `ObjectRef`s
// rewritten by the moving collector via `update_after_gc`. We never retain a
// raw object address in this table; dedup compares against the current address
// resolved from the GC-updated global refs.
//
// On top of that we add a **generation/liveness** layer keyed by a
// monotonically increasing per-VM counter: tokens are never reused, so a token
// for a destroyed table (or one the host invented) is rejected by a table miss
// rather than aliasing a live entry (no ABA). Identical object refs dedup to a
// single token with a reference count, so repeated returns of one object do not
// grow the table and `cratonvm_release_ref` can release each returned token.
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::OnceLock;

use cratonvm_vm::native::jni::JObject;
use cratonvm_vm::SharedVm;

/// One live opaque object token.
struct VmHandleEntry {
    /// The `JniGlobalRefs` handle that owns the GC-rooted `ObjectRef`.
    gref: JObject,
    /// Number of live API returns that handed this token to the host.
    refs: usize,
}

/// One VM's opaque-token to global-ref mapping.
#[derive(Default)]
struct VmHandleTable {
    /// Next token to hand out. Starts at 1 (`0` is the reserved null token) and
    /// only ever increases, so a token is never reused for a different object.
    next_token: u64,
    /// token -> GC-rooted entry.
    by_token: HashMap<u64, VmHandleEntry>,
}

/// Process-global registry of per-VM handle tables, keyed by the `SharedVm`'s
/// stable address (each `SharedVm` lives behind an `Arc` for the VM's life).
/// `cratonvm_destroy` drops the VM's entry, after which its tokens no longer
/// resolve.
static HANDLE_TABLES: OnceLock<Mutex<HashMap<usize, VmHandleTable>>> = OnceLock::new();

fn handle_tables() -> &'static Mutex<HashMap<usize, VmHandleTable>> {
    HANDLE_TABLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stable per-VM key (the `SharedVm`'s address).
fn vm_key(shared: &SharedVm) -> usize {
    shared as *const SharedVm as usize
}

/// `Option<ObjectRef>` → `CratonRef` opaque token (`0` == null).
///
/// Registers a non-null object in `shared`'s `JniGlobalRefs` (making it a GC
/// root with moving-GC pointer fixup) and mints / reuses an opaque token for it.
fn register_handle(shared: &SharedVm, o: Option<ObjectRef>) -> CratonRef {
    let oref = match o {
        Some(r) if !r.as_ptr().is_null() => r,
        _ => return 0,
    };
    let mut tables = match handle_tables().lock() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let table = tables.entry(vm_key(shared)).or_default();
    let mut grefs = shared.natives.jni_global_refs.lock();
    // Dedup by resolving each global ref to its current post-GC address. This
    // keeps the table correct when a moving collector rewrites the global refs.
    for (&tok, entry) in &mut table.by_token {
        if grefs
            .resolve(entry.gref)
            .is_some_and(|current| current.as_ptr() == oref.as_ptr())
        {
            entry.refs = entry.refs.saturating_add(1);
            return tok;
        }
    }
    // Park the object as a GC root and mint a fresh, never-reused token.
    let gref = grefs.add(oref);
    table.next_token = table.next_token.checked_add(1).unwrap_or(1).max(1);
    let tok = table.next_token;
    table.by_token.insert(tok, VmHandleEntry { gref, refs: 1 });
    tok
}

/// `CratonRef` opaque token → `Option<ObjectRef>` (`0` == null).
///
/// Validates the token against `shared`'s per-VM table and the underlying
/// `JniGlobalRefs`; an unknown, stale, or fabricated token resolves to `None`
/// (rejected) rather than being turned into a raw pointer dereference.
fn resolve_handle(shared: &SharedVm, h: CratonRef) -> Option<ObjectRef> {
    if h == 0 {
        return None;
    }
    let tables = handle_tables().lock().ok()?;
    let gref = tables.get(&vm_key(shared))?.by_token.get(&h)?.gref;
    // `JniGlobalRefs::resolve` validates the gref is still live and returns the
    // current (post-GC) address.
    shared.natives.jni_global_refs.lock().resolve(gref)
}

fn release_handle(shared: &SharedVm, h: CratonRef) -> bool {
    if h == 0 {
        return true;
    }
    let mut tables = match handle_tables().lock() {
        Ok(tables) => tables,
        Err(_) => return false,
    };
    let Some(table) = tables.get_mut(&vm_key(shared)) else {
        return false;
    };
    let Some(entry) = table.by_token.get_mut(&h) else {
        return false;
    };
    if entry.refs > 1 {
        entry.refs -= 1;
        return true;
    }
    let entry = table
        .by_token
        .remove(&h)
        .expect("entry was present while releasing handle");
    shared.natives.jni_global_refs.lock().remove(entry.gref)
}

fn decode_craton_args(api: &str, shared: &SharedVm, args: &[CratonValue]) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(args.len());
    for (idx, arg) in args.iter().copied().enumerate() {
        match arg.to_value(shared) {
            Ok(v) => out.push(v),
            Err(e) => {
                set_last_error(format!("{api}: invalid argument {idx}: {e}"));
                return None;
            }
        }
    }
    Some(out)
}

fn decode_craton_value(api: &str, shared: &SharedVm, value: CratonValue) -> Option<Value> {
    match value.to_value(shared) {
        Ok(v) => Some(v),
        Err(e) => {
            set_last_error(format!("{api}: invalid value: {e}"));
            None
        }
    }
}

/// Drop a VM's handle table (called from `cratonvm_destroy`). Releases the
/// underlying global refs so the parked objects are no longer GC roots.
fn drop_handle_table(shared: &SharedVm) {
    if let Ok(mut tables) = handle_tables().lock() {
        if let Some(table) = tables.remove(&vm_key(shared)) {
            let mut grefs = shared.natives.jni_global_refs.lock();
            for entry in table.by_token.into_values() {
                grefs.remove(entry.gref);
            }
        }
    }
}

// --- thread-local last-error -----------------------------------------------

thread_local! {
    /// Last error message for the calling thread, as a NUL-terminated C string.
    /// `None` means "no pending error". Mirrors HotSpot/JNI's
    /// per-thread pending-exception model: an error set on one thread is not
    /// visible on another.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Record `msg` as this thread's last error (lossily NUL-sanitised so an
/// interior NUL cannot truncate or panic the conversion).
fn set_last_error(msg: impl Into<Vec<u8>>) {
    let mut bytes = msg.into();
    bytes.retain(|&b| b != 0);
    let c = CString::new(bytes).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

/// Clear this thread's last error.
fn clear_last_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

// --- create / destroy ------------------------------------------------------

/// `CratonVm *cratonvm_create(const JavaVMInitArgs *args)`
///
/// Build and bootstrap a VM, returning an owning opaque handle. `args` may be
/// null (defaults are used). Reuses the exact `Vm::new` + [`bootstrap`] +
/// `set_jni_context_arc` path the Invocation API uses, so the returned handle's
/// thread may immediately invoke. Returns null on failure (and sets the
/// thread's last error, retrievable via [`cratonvm_last_error`]). Non-null
/// init args must request a supported JNI version; unrecognized or malformed
/// options fail unless `ignoreUnrecognized` is non-zero.
/// Fails if a JNI Invocation-API VM or another flat `CratonVm` is already
/// active in this process.
///
/// The returned pointer must be released with [`cratonvm_destroy`].
///
/// The compatibility mode is whatever the options say: `--jdk-only` selects
/// [`CompatibilityMode::JdkOnly`], and its absence leaves
/// [`DEFAULT_COMPATIBILITY_MODE`]. Use
/// [`cratonvm_create_with_compatibility`] to state the policy as a typed
/// argument instead.
///
/// # Safety
/// `args`, when non-null, must be a valid `*const JavaVMInitArgs`.
#[no_mangle]
pub extern "C" fn cratonvm_create(args: *const JavaVMInitArgs) -> *mut CratonVm {
    create_vm("cratonvm_create", args, None)
}

/// `CratonVm *cratonvm_create_with_compatibility(const JavaVMInitArgs *args,
/// cratonvm_jint compatibility_mode)`
///
/// [`cratonvm_create`] with the compatibility mode stated as an explicit
/// `CRATONVM_COMPATIBILITY_*` value instead of an option string. Everything
/// else — args parsing, bootstrap, handle ownership, failure reporting — is
/// identical; only the policy source differs.
///
/// This exists because a C host has no command line, and a `JavaVMInitArgs` is
/// often assembled far from the call site that knows the policy (a config
/// file, a plugin manifest, an outer application). A typed argument lets the
/// call site state it directly.
///
/// `compatibility_mode` must be [`CRATONVM_COMPATIBILITY_COMPATIBLE`] or
/// [`CRATONVM_COMPATIBILITY_JDK_ONLY`]. Any other integer is **rejected**
/// (null + last error), never clamped to the compatible default — a host built
/// against a newer header is told, rather than silently getting the loose
/// policy it thought it had opted out of. Probe support without paying for a
/// failed create with [`cratonvm_compatibility_mode_supported`].
///
/// Passing [`CRATONVM_COMPATIBILITY_COMPATIBLE`] while the options also carry
/// `--jdk-only` is a contradiction error, not a precedence rule; passing
/// [`CRATONVM_COMPATIBILITY_JDK_ONLY`] alongside `--jdk-only` agrees and is
/// accepted. See [`apply_compatibility`].
///
/// # Safety
/// `args`, when non-null, must be a valid `*const JavaVMInitArgs`.
#[no_mangle]
pub extern "C" fn cratonvm_create_with_compatibility(
    args: *const JavaVMInitArgs,
    compatibility_mode: JInt,
) -> *mut CratonVm {
    const API: &str = "cratonvm_create_with_compatibility";
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        compatibility_mode_from_abi(compatibility_mode)
    }));
    let requested = match decoded {
        Ok(Ok(mode)) => mode,
        Ok(Err(e)) => {
            set_last_error(format!("{API}: {e}"));
            return std::ptr::null_mut();
        }
        Err(_) => {
            set_last_error(format!("{API}: panic while validating compatibility mode"));
            return std::ptr::null_mut();
        }
    };
    create_vm(API, args, Some(requested))
}

/// The shared body of [`cratonvm_create`] and
/// [`cratonvm_create_with_compatibility`].
///
/// `api` is the *calling* entry point's name, threaded through so every
/// last-error message names the function the host actually called rather than
/// this shared helper — the host reads that prefix, and "cratonvm_create: …"
/// after a `cratonvm_create_with_compatibility` call would send it looking in
/// the wrong place.
///
/// # Safety
/// `args`, when non-null, must be a valid `*const JavaVMInitArgs`.
fn create_vm(
    api: &str,
    args: *const JavaVMInitArgs,
    requested: Option<CompatibilityMode>,
) -> *mut CratonVm {
    let result = catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        let created_guard = match CREATED_VM.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(format!("{api}: VM registry is poisoned"));
                return std::ptr::null_mut();
            }
        };
        if created_guard.is_some() {
            set_last_error(format!("{api}: a JNI Invocation API VM is already active"));
            return std::ptr::null_mut();
        }
        let mut flat_guard = match begin_flat_vm_create() {
            Ok(g) => g,
            Err(e) => {
                set_last_error(format!("{api}: {e}"));
                return std::ptr::null_mut();
            }
        };
        // SAFETY: caller contract — `args` is a valid `*const JavaVMInitArgs`
        // or null (handled inside `config_from_args_with_compatibility`).
        let config = match unsafe { config_from_args_with_compatibility(args, requested) } {
            Ok(config) => config,
            Err(e) => {
                set_last_error(format!("{api}: {e}"));
                return std::ptr::null_mut();
            }
        };
        let mut vm = Vm::new(config);
        bootstrap(&mut vm);
        // (`Vm::new` already published the process-global VM cell.)
        // Publish this thread's JNI context so JNIEnv-table calls on the
        // creating thread resolve this VM (parity with JNI_CreateJavaVM).
        set_jni_context_arc(vm.shared.get_arc());
        let mut boxed = Box::new(CratonVm { vm });
        let vm_key = (&mut *boxed as *mut CratonVm) as usize;
        if let Err(e) = flat_guard.activate(vm_key) {
            set_last_error(format!("{api}: {e}"));
            return std::ptr::null_mut();
        }
        Box::into_raw(boxed)
    }));
    match result {
        Ok(ptr) => ptr,
        Err(_) => {
            set_last_error(format!("{api}: panic during VM bootstrap"));
            std::ptr::null_mut()
        }
    }
}

/// `void cratonvm_destroy(CratonVm *vm)`
///
/// Drop the VM and free the handle. Passing null is a no-op. After this call
/// the handle is dangling and must not be reused.
///
/// # Safety
/// `vm` must be a handle returned by [`cratonvm_create`] that has not already
/// been destroyed, or null.
#[no_mangle]
pub extern "C" fn cratonvm_destroy(vm: *mut CratonVm) {
    if vm.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let vm_key = vm as usize;
        // SAFETY: caller contract — `vm` came from `cratonvm_create` and is not
        // double-freed. Reclaim the Box and drop it.
        let boxed = unsafe { Box::from_raw(vm) };
        // Release this VM's opaque-handle table (and the global refs that pinned
        // its objects as GC roots) so they are not leaked for the process life.
        drop_handle_table(&boxed.vm.shared);
        // Clear this thread's JNI TLS context: `cratonvm_create` published it to
        // point at this VM, and once the VM is dropped that `Arc<SharedVm>` would
        // otherwise dangle in TLS — a later JNIEnv-table call on this thread
        // would resolve a freed VM. Clearing drops the TLS `Arc` for this thread.
        clear_jni_context();
        drop(boxed);
        release_flat_vm(vm_key);
    }));
}

/// `jint cratonvm_release_ref(CratonVm *vm, CratonRef reference)`
///
/// Release one live object/string/throwable token previously returned by this
/// VM. Passing `0` (Java null) is a no-op success. Releasing a nonzero token
/// invalidates one returned handle; when all returns of that token have been
/// balanced, the backing JNI global ref is removed and the object is no longer
/// pinned as a GC root by libcratonvm.
///
/// # Safety
/// `vm` must be a handle returned by [`cratonvm_create`] that has not already
/// been destroyed, or null. `reference` must be `0` or a live [`CratonRef`]
/// returned by this VM and not already fully released.
#[no_mangle]
pub extern "C" fn cratonvm_release_ref(vm: *mut CratonVm, reference: CratonRef) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                if release_handle(&h.vm.shared, reference) {
                    JNI_OK
                } else {
                    set_last_error(format!(
                        "cratonvm_release_ref: stale or unknown CratonRef object token 0x{reference:x}"
                    ));
                    JNI_ERR
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_release_ref: panic");
        JNI_ERR
    })
}

// --- helpers to deref the handle safely ------------------------------------

/// Borrow the `CratonVm` behind a raw handle, or return `err_val` after setting
/// the last error, when the handle is null.
///
/// # Safety
/// `vm` must be a valid `*mut CratonVm` or null.
unsafe fn with_vm<R>(vm: *mut CratonVm, err_val: R, f: impl FnOnce(&mut CratonVm) -> R) -> R {
    if vm.is_null() {
        set_last_error("null CratonVm handle");
        return err_val;
    }
    // Reentrancy guard: forming `&mut *vm` while an outer `with_vm` for the same
    // handle is already on the stack (e.g. a flat-API call made from Java code
    // the VM is currently executing) would create two aliasing `&mut CratonVm`
    // — instant UB. Mark the handle in-use for the duration of `f`; a re-entrant
    // call with the same handle is rejected before any borrow is formed. The
    // flag lives in a side table (not in the borrowed struct), so checking it
    // never aliases the live `&mut`.
    let _borrow = match BorrowGuard::acquire(vm) {
        Some(g) => g,
        None => {
            set_last_error("re-entrant CratonVm access on the same handle is not allowed");
            return err_val;
        }
    };
    // SAFETY: caller contract — `vm` is a live handle; the borrow guard above
    // guarantees no other `&mut CratonVm` for this handle is live, so this is
    // the unique `&mut` for the duration of `f`.
    let handle = unsafe { &mut *vm };
    set_jni_context_arc(handle.vm.shared.get_arc());
    f(handle)
}

/// Process-global set of `CratonVm` handles currently borrowed by an in-flight
/// `with_vm`. Used purely as a reentrancy flag; the address is never
/// dereferenced through this table.
static BORROWED_HANDLES: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();

fn borrowed_handles() -> &'static Mutex<std::collections::HashSet<usize>> {
    BORROWED_HANDLES.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// RAII reentrancy guard: marks a handle in-use on `acquire` and clears it on
/// drop (including on panic), so a re-entrant `with_vm` on the same handle is
/// rejected for the lifetime of the guard.
struct BorrowGuard(usize);

impl BorrowGuard {
    /// Returns `Some(guard)` if the handle was free (now marked in-use), or
    /// `None` if it is already borrowed (re-entrant access).
    fn acquire(vm: *mut CratonVm) -> Option<Self> {
        let key = vm as usize;
        let mut set = borrowed_handles().lock().ok()?;
        if set.insert(key) {
            Some(BorrowGuard(key))
        } else {
            None
        }
    }
}

impl Drop for BorrowGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = borrowed_handles().lock() {
            set.remove(&self.0);
        }
    }
}

// --- load_class ------------------------------------------------------------

/// `CratonClass cratonvm_load_class(CratonVm *vm, const char *name)`
///
/// Load (and link) a class by its internal name (`"java/lang/System"`), writing
/// the resolved [`CratonClass`] handle into `*out_class`. Returns [`JNI_OK`] on
/// success or [`JNI_ERR`] on failure (bad handle, null `name`, non-UTF-8 name,
/// or a load/link error) — on failure the thread's last error is set and
/// `*out_class` is left untouched.
///
/// (A separate out-pointer + return code is used rather than a sentinel handle
/// because `0` is a legitimate `ClassId`.)
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `name` is a valid NUL-terminated
/// C string; `out_class`, when non-null, is writable.
#[no_mangle]
pub extern "C" fn cratonvm_load_class(
    vm: *mut CratonVm,
    name: *const c_char,
    out_class: *mut CratonClass,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                if name.is_null() {
                    set_last_error("cratonvm_load_class: null class name");
                    return JNI_ERR;
                }
                // SAFETY: caller contract — `name` is a valid C string.
                let name = match CStr::from_ptr(name).to_str() {
                    Ok(s) => s,
                    Err(_) => {
                        set_last_error("cratonvm_load_class: class name is not valid UTF-8");
                        return JNI_ERR;
                    }
                };
                match h.vm.load_class(name) {
                    Ok(class_id) => {
                        if !out_class.is_null() {
                            // SAFETY: `out_class` checked non-null; caller
                            // contract says it is writable.
                            *out_class = class_id.as_u32() as CratonClass;
                        }
                        JNI_OK
                    }
                    Err(e) => {
                        set_last_error(format!("cratonvm_load_class({name}): {e}"));
                        JNI_ERR
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_load_class: panic");
        JNI_ERR
    })
}

// --- invoke_static ---------------------------------------------------------

/// `CratonValue cratonvm_invoke_static(CratonVm *vm, const char *class,
///     const char *method, const char *sig, const CratonValue *args, int32_t n_args)`
///
/// Invoke a static method by class name, method name, and JVM descriptor
/// (`sig`, e.g. `"(I)I"`). Arguments are a typed [`CratonValue`] array of length
/// `n_args` (`args` may be null when `n_args == 0`).
///
/// Returns a [`CratonValue`]: the method's return value (tag `VOID` for a
/// `void` method), or `tag == craton_tag::ERROR` on any failure — in which case
/// [`cratonvm_last_error`] holds a message (for a thrown Java exception it
/// records the throwable, for an internal error the VM error text).
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `class`/`method`/`sig` are valid
/// NUL-terminated C strings; `args` points to `n_args` valid [`CratonValue`]s
/// (or is null when `n_args == 0`).
#[no_mangle]
pub extern "C" fn cratonvm_invoke_static(
    vm: *mut CratonVm,
    class: *const c_char,
    method: *const c_char,
    sig: *const c_char,
    args: *const CratonValue,
    n_args: JInt,
) -> CratonValue {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, CratonValue::error(), |h| {
                // SAFETY: caller contract — these are valid C strings or null.
                let (class, method, sig) = match (
                    cstr_or_err(class, "class name"),
                    cstr_or_err(method, "method name"),
                    cstr_or_err(sig, "method signature"),
                ) {
                    (Some(c), Some(m), Some(s)) => (c, m, s),
                    _ => return CratonValue::error(),
                };

                if n_args < 0 || (n_args > 0 && args.is_null()) {
                    set_last_error("cratonvm_invoke_static: bad args array");
                    return CratonValue::error();
                }
                let in_args: &[CratonValue] = if n_args == 0 {
                    &[]
                } else {
                    // SAFETY: checked `args` non-null and `n_args > 0` above.
                    std::slice::from_raw_parts(args, n_args as usize)
                };
                let values =
                    match decode_craton_args("cratonvm_invoke_static", &h.vm.shared, in_args) {
                        Some(values) => values,
                        None => return CratonValue::error(),
                    };

                match h.vm.invoke(class, method, sig, &values) {
                    Ok(Some(v)) => CratonValue::from_value(&h.vm.shared, v),
                    Ok(None) => CratonValue::void(),
                    Err(e) => {
                        set_last_error(describe_failure(&e));
                        CratonValue::error()
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_invoke_static: panic");
        CratonValue::error()
    })
}

/// Read a `*const c_char` into a `&str`, or set the last error and return None.
///
/// # Safety
/// `p`, when non-null, is a valid NUL-terminated C string.
unsafe fn cstr_or_err<'a>(p: *const c_char, what: &str) -> Option<&'a str> {
    if p.is_null() {
        set_last_error(format!("null {what}"));
        return None;
    }
    // SAFETY: caller contract — `p` is a valid C string.
    match unsafe { CStr::from_ptr(p) }.to_str() {
        Ok(s) => Some(s),
        Err(_) => {
            set_last_error(format!("{what} is not valid UTF-8"));
            None
        }
    }
}

/// Human-readable text for a [`MethodCallFailed`]. Internal errors delegate to
/// `VmError`'s self-describing `Display`; a thrown Java exception is reported
/// with its throwable handle (reading the throwable's message/class would
/// require heap-header access owned by another work item, so it is left as a
/// handle the host can inspect via the JNIEnv table).
fn describe_failure(e: &MethodCallFailed) -> String {
    match e {
        MethodCallFailed::InternalError(err) => err.to_string(),
        MethodCallFailed::ExceptionThrown(obj) => {
            format!(
                "java exception thrown (throwable handle=0x{:x})",
                obj.as_ptr() as u64
            )
        }
    }
}

// --- new_string ------------------------------------------------------------

/// `CratonRef cratonvm_new_string(CratonVm *vm, const char *utf8)`
///
/// Create an interned `java.lang.String` from a UTF-8 C string, returning its
/// [`CratonRef`] handle (suitable as a `CratonValue` `OBJECT` argument to
/// [`cratonvm_invoke_static`]). Returns `0` (null handle) on failure (bad VM
/// handle, null/invalid `utf8`) and sets the thread's last error.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `utf8` is a valid NUL-terminated
/// UTF-8 C string.
#[no_mangle]
pub extern "C" fn cratonvm_new_string(vm: *mut CratonVm, utf8: *const c_char) -> CratonRef {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, 0u64, |h| {
                // SAFETY: caller contract — `utf8` is a valid C string or null.
                let text = match cstr_or_err(utf8, "string") {
                    Some(s) => s,
                    None => return 0u64,
                };
                // Reuse the VM's interning string constructor (Layer-1
                // `vm::create_java_string`), the same one the JNIEnv table and
                // the interpreter use for `ldc` of a literal.
                let obj = cratonvm_vm::vm::create_java_string(&h.vm.shared, text);
                register_handle(&h.vm.shared, Some(obj))
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_new_string: panic");
        0u64
    })
}

// --- string_utf8 read-back -------------------------------------------------

/// `char *cratonvm_string_utf8(CratonVm *vm, CratonRef str)`
///
/// Read a `java.lang.String` handle (e.g. an `OBJECT` result from
/// [`cratonvm_invoke_static`]) into a freshly-allocated, NUL-terminated UTF-8 C
/// string — the read-back companion to [`cratonvm_new_string`]. Returns null
/// (and sets the thread's last error) on a bad VM/handle or when `str` is not a
/// `String`. The returned buffer is owned by the CALLER and must be released
/// with [`cratonvm_free_string`] — it is NOT the thread-local last-error buffer.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `str` is a [`CratonRef`] the VM
/// previously handed out (or `0`).
#[no_mangle]
pub extern "C" fn cratonvm_string_utf8(vm: *mut CratonVm, str: CratonRef) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, std::ptr::null_mut(), |h| {
                let oref = match resolve_handle(&h.vm.shared, str) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_string_utf8: null string handle");
                        return std::ptr::null_mut();
                    }
                };
                // Reuse the VM's String reader (`vm::read_java_string`), the same
                // primitive the JNIEnv `GetStringUTFChars` slot uses.
                match cratonvm_vm::vm::read_java_string(&h.vm.shared.mem.heap, oref) {
                    Some(s) => {
                        // Strip interior NULs so `CString::new` cannot fail; the
                        // buffer is caller-owned (freed via cratonvm_free_string).
                        let mut bytes = s.into_bytes();
                        bytes.retain(|&b| b != 0);
                        match CString::new(bytes) {
                            Ok(c) => c.into_raw(),
                            Err(_) => std::ptr::null_mut(),
                        }
                    }
                    None => {
                        set_last_error("cratonvm_string_utf8: handle is not a java.lang.String");
                        std::ptr::null_mut()
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_string_utf8: panic");
        std::ptr::null_mut()
    })
}

/// `void cratonvm_free_string(char *s)`
///
/// Release a buffer returned by [`cratonvm_string_utf8`]. Passing null is a
/// no-op; must not be called on any other pointer.
///
/// # Safety
/// `s` is null or a pointer returned by [`cratonvm_string_utf8`] that has not
/// already been freed.
#[no_mangle]
pub extern "C" fn cratonvm_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: caller contract — `s` came from cratonvm_string_utf8
    // (`CString::into_raw`) and is freed exactly once.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(CString::from_raw(s));
    }));
}

// --- compatibility mode ----------------------------------------------------

/// `cratonvm_jint cratonvm_compatibility_mode(CratonVm *vm)`
///
/// The compatibility mode a **live** VM is actually running under, as a
/// `CRATONVM_COMPATIBILITY_*` value. Returns `-1` — never a mode value, since
/// the ABI encoding is append-only from `0` upwards — on a null or otherwise
/// unusable handle, with the reason in [`cratonvm_last_error`].
///
/// This is a read-back, not an echo of the request: a host that assembled its
/// options in one place and its policy argument in another can confirm what it
/// actually got instead of trusting what it asked for.
#[no_mangle]
pub extern "C" fn cratonvm_compatibility_mode(vm: *mut CratonVm) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, -1, |h| {
                compatibility_mode_to_abi(h.vm.shared.config.compatibility_mode)
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_compatibility_mode: panic");
        -1
    })
}

/// `cratonvm_jint cratonvm_compatibility_mode_supported(cratonvm_jint mode)`
///
/// Capability probe. **Needs no VM**, so it answers "does this build know the
/// mode?" without the trial-and-error of a failed create — and, combined with
/// `dlsym`, it also covers libraries that predate the symbol entirely.
///
/// Returns:
///
/// * `1` — this build understands `mode` and honours it;
/// * `-1` — `mode` is not a `CRATONVM_COMPATIBILITY_*` value at all;
/// * `0` — reserved for a mode this build *knows* but cannot honour (nothing
///   returns it today; it exists so a future build can distinguish "never heard
///   of it" from "recognised but unavailable here" without renumbering).
///
/// Like every other entry point it clears this thread's pending error on entry;
/// it never sets one.
#[no_mangle]
pub extern "C" fn cratonvm_compatibility_mode_supported(mode: JInt) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        match compatibility_mode_from_abi(mode) {
            Ok(_) => 1,
            Err(_) => -1,
        }
    }))
    .unwrap_or(-1)
}

// --- last_error ------------------------------------------------------------

/// `const char *cratonvm_last_error(CratonVm *vm)`
///
/// Return this thread's pending error message as a NUL-terminated C string, or
/// null if there is none. The `vm` parameter is accepted for API symmetry and
/// future per-VM scoping but is not dereferenced — the error state is
/// thread-local (mirroring JNI's per-thread pending exception).
///
/// The returned pointer is owned by the library and valid until the next
/// `cratonvm_*` call on this thread (each entry point clears the error on
/// entry) or a [`cratonvm_clear_error`] call. The host should copy it if it
/// needs to outlive that window.
///
/// # Safety
/// `vm` is ignored; any value (including null) is accepted.
#[no_mangle]
pub extern "C" fn cratonvm_last_error(_vm: *mut CratonVm) -> *const c_char {
    catch_unwind(AssertUnwindSafe(|| {
        LAST_ERROR.with(|e| match e.borrow().as_ref() {
            Some(c) => c.as_ptr(),
            None => std::ptr::null(),
        })
    }))
    .unwrap_or(std::ptr::null())
}

/// `void cratonvm_clear_error(CratonVm *vm)`
///
/// Clear this thread's pending error (invalidating any pointer previously
/// returned by [`cratonvm_last_error`]). `vm` is ignored (thread-local state).
///
/// # Safety
/// `vm` is ignored; any value (including null) is accepted.
#[no_mangle]
pub extern "C" fn cratonvm_clear_error(_vm: *mut CratonVm) {
    let _ = catch_unwind(AssertUnwindSafe(clear_last_error));
}

// --- invoke_virtual --------------------------------------------------------

/// `CratonValue cratonvm_invoke_virtual(CratonVm *vm, CratonRef receiver,
///     const char *method, const char *sig, const CratonValue *args, int32_t n_args)`
///
/// Invoke an instance method on `receiver` via **virtual dispatch** — the method
/// is resolved against the receiver's *runtime* class (the most-derived
/// override), exactly like an `invokevirtual` / `invokeinterface` bytecode. This
/// is the instance-method companion to [`cratonvm_invoke_static`].
///
/// * `receiver` is a non-null object handle ([`CratonRef`]) the VM previously
///   handed out (e.g. from [`cratonvm_new_string`] or an `OBJECT` result).
/// * `sig` is the method's JVM descriptor and **excludes the receiver**
///   (e.g. `"(I)Ljava/lang/String;"`), matching how the interpreter stores
///   instance-method descriptors; the receiver is supplied via `receiver`, not
///   in `args`.
/// * `args` is a typed [`CratonValue`] array of length `n_args` (may be null
///   when `n_args == 0`).
///
/// Returns the method's return value ([`craton_tag::VOID`] for a `void` method)
/// or `tag == craton_tag::ERROR` on any failure (null/bad receiver, bad strings,
/// a thrown Java exception, or an internal error) — in which case
/// [`cratonvm_last_error`] holds a message.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `receiver` is a live [`CratonRef`];
/// `method`/`sig` are valid NUL-terminated C strings; `args` points to `n_args`
/// valid [`CratonValue`]s (or is null when `n_args == 0`).
#[no_mangle]
pub extern "C" fn cratonvm_invoke_virtual(
    vm: *mut CratonVm,
    receiver: CratonRef,
    method: *const c_char,
    sig: *const c_char,
    args: *const CratonValue,
    n_args: JInt,
) -> CratonValue {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, CratonValue::error(), |h| {
                let recv = match resolve_handle(&h.vm.shared, receiver) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_invoke_virtual: null receiver handle");
                        return CratonValue::error();
                    }
                };
                // SAFETY: caller contract — these are valid C strings or null.
                let (method, sig) = match (
                    cstr_or_err(method, "method name"),
                    cstr_or_err(sig, "method signature"),
                ) {
                    (Some(m), Some(s)) => (m, s),
                    _ => return CratonValue::error(),
                };

                if n_args < 0 || (n_args > 0 && args.is_null()) {
                    set_last_error("cratonvm_invoke_virtual: bad args array");
                    return CratonValue::error();
                }
                let in_args: &[CratonValue] = if n_args == 0 {
                    &[]
                } else {
                    // SAFETY: checked `args` non-null and `n_args > 0` above.
                    std::slice::from_raw_parts(args, n_args as usize)
                };

                // Resolve the receiver's *runtime* class — invoking on the
                // most-derived class is exactly virtual dispatch (the same
                // pattern `Vm::run_pending_finalizers` uses to virtual-dispatch
                // `finalize()` on an object's concrete class).
                let class_id = h.vm.shared.mem.heap.class_id_of(recv);
                let class_name = match h.vm.class_name(class_id) {
                    Some(n) => n,
                    None => {
                        set_last_error("cratonvm_invoke_virtual: receiver has no resolvable class");
                        return CratonValue::error();
                    }
                };

                // The receiver is arg 0 (descriptor excludes it); typed args follow.
                let mut values: Vec<Value> = Vec::with_capacity(in_args.len() + 1);
                values.push(Value::Object(Some(recv)));
                let decoded_args =
                    match decode_craton_args("cratonvm_invoke_virtual", &h.vm.shared, in_args) {
                        Some(values) => values,
                        None => return CratonValue::error(),
                    };
                values.extend(decoded_args);

                match h.vm.invoke(&class_name, method, sig, &values) {
                    Ok(Some(v)) => CratonValue::from_value(&h.vm.shared, v),
                    Ok(None) => CratonValue::void(),
                    Err(e) => {
                        set_last_error(describe_failure(&e));
                        CratonValue::error()
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_invoke_virtual: panic");
        CratonValue::error()
    })
}

// --- object inspection / field read-back -----------------------------------

/// `CratonClass cratonvm_object_class(CratonVm *vm, CratonRef obj, CratonClass *out_class)`
///
/// Write the **runtime class** handle of `obj` into `*out_class` and return
/// [`JNI_OK`]; returns [`JNI_ERR`] (with the last error set, `*out_class`
/// untouched) on a bad VM/handle. An out-pointer + return code is used rather
/// than a sentinel because `0` is a valid [`CratonClass`] (`java/lang/Object`).
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `obj` is a live [`CratonRef`];
/// `out_class`, when non-null, is writable.
#[no_mangle]
pub extern "C" fn cratonvm_object_class(
    vm: *mut CratonVm,
    obj: CratonRef,
    out_class: *mut CratonClass,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                let oref = match resolve_handle(&h.vm.shared, obj) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_object_class: null object handle");
                        return JNI_ERR;
                    }
                };
                let class_id = h.vm.shared.mem.heap.class_id_of(oref);
                if !out_class.is_null() {
                    // SAFETY: `out_class` checked non-null; writable per contract.
                    *out_class = class_id.as_u32() as CratonClass;
                }
                JNI_OK
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_object_class: panic");
        JNI_ERR
    })
}

/// `char *cratonvm_class_name(CratonVm *vm, CratonClass class)`
///
/// Read a class handle's internal name (`"java/lang/String"`) into a freshly
/// allocated, NUL-terminated UTF-8 C string. Returns null (with the last error
/// set) on a bad VM handle or an unresolvable class. The returned buffer is
/// caller-owned and must be released with [`cratonvm_free_string`] (same
/// ownership split as [`cratonvm_string_utf8`]).
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `class` is a [`CratonClass`] the VM
/// previously handed out.
#[no_mangle]
pub extern "C" fn cratonvm_class_name(vm: *mut CratonVm, class: CratonClass) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, std::ptr::null_mut(), |h| {
                let class_id = ClassId::new(class as u32);
                match h.vm.class_name(class_id) {
                    Some(name) => {
                        let mut bytes = name.into_bytes();
                        bytes.retain(|&b| b != 0);
                        match CString::new(bytes) {
                            Ok(c) => c.into_raw(),
                            Err(_) => std::ptr::null_mut(),
                        }
                    }
                    None => {
                        set_last_error("cratonvm_class_name: unresolvable class handle");
                        std::ptr::null_mut()
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_class_name: panic");
        std::ptr::null_mut()
    })
}

/// `int32_t cratonvm_field_count(CratonVm *vm, CratonRef obj)`
///
/// Return the number of instance-field slots in `obj`'s class layout — the valid
/// `index` range `[0, count)` for [`cratonvm_get_field`]. Returns `-1` (with the
/// last error set) on a bad VM/handle.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `obj` is a live [`CratonRef`].
#[no_mangle]
pub extern "C" fn cratonvm_field_count(vm: *mut CratonVm, obj: CratonRef) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, -1, |h| {
                let oref = match resolve_handle(&h.vm.shared, obj) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_field_count: null object handle");
                        return -1;
                    }
                };
                let class_id = h.vm.shared.mem.heap.class_id_of(oref);
                let n =
                    h.vm.shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields)
                        .unwrap_or(0);
                n as JInt
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_field_count: panic");
        -1
    })
}

/// `CratonValue cratonvm_get_field(CratonVm *vm, CratonRef obj, int32_t index)`
///
/// Read instance field slot `index` of `obj` (an index in `[0,
/// cratonvm_field_count(obj))`) as a typed [`CratonValue`] — the object-field
/// read-back companion to [`cratonvm_string_utf8`]'s string read-back. The slot
/// is resolved by **layout index**, not by name: combine with the JNIEnv
/// reflection table, [`cratonvm_invoke_virtual`] of a getter, or a host-side
/// descriptor map to map a field name → index. (Name-based field resolution
/// needs a class-layout-by-name accessor not yet exposed by the VM — a noted
/// follow-up.)
///
/// Returns `tag == craton_tag::ERROR` (with the last error set) on a bad
/// VM/handle or an out-of-range `index`.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `obj` is a live [`CratonRef`].
#[no_mangle]
pub extern "C" fn cratonvm_get_field(
    vm: *mut CratonVm,
    obj: CratonRef,
    index: JInt,
) -> CratonValue {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, CratonValue::error(), |h| {
                let oref = match resolve_handle(&h.vm.shared, obj) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_get_field: null object handle");
                        return CratonValue::error();
                    }
                };
                let class_id = h.vm.shared.mem.heap.class_id_of(oref);
                let nfields =
                    h.vm.shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields)
                        .unwrap_or(0);
                if index < 0 || (index as usize) >= nfields {
                    set_last_error(format!(
                        "cratonvm_get_field: index {index} out of range for {nfields} field(s)"
                    ));
                    return CratonValue::error();
                }
                // Bounds-checked above; `heap.get_field` reads the slot value.
                let v = h.vm.shared.mem.heap.get_field(oref, index as usize);
                CratonValue::from_value(&h.vm.shared, v)
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_get_field: panic");
        CratonValue::error()
    })
}

/// `int32_t cratonvm_field_index(CratonVm *vm, CratonClass cls, const char *name, int32_t *out_index)`
///
/// Resolve the **name** of an instance field on `cls` to its layout slot index
/// (walking the superclass chain; most-derived declaration wins), writing it to
/// `*out_index`. Returns [`JNI_OK`], or [`JNI_ERR`] (with the last error set,
/// `*out_index` untouched) when the class is unloaded or has no such instance
/// field. The resolved index is usable with [`cratonvm_get_field`] /
/// [`cratonvm_set_field`]. (Resolution is by name; for a field shadowed by a
/// same-name field in a subclass, the most-derived one wins.)
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `cls` is a [`CratonClass`] the VM
/// handed out; `name` is a valid NUL-terminated C string; `out_index`, when
/// non-null, is writable.
#[no_mangle]
pub extern "C" fn cratonvm_field_index(
    vm: *mut CratonVm,
    cls: CratonClass,
    name: *const c_char,
    out_index: *mut JInt,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                // SAFETY: caller contract — `name` is a valid C string or null.
                let field_name = match cstr_or_err(name, "field name") {
                    Some(s) => s,
                    None => return JNI_ERR,
                };
                let class_id = ClassId::new(cls as u32);
                match h.vm.instance_field_index(class_id, field_name) {
                    Some(idx) => {
                        if !out_index.is_null() {
                            // SAFETY: `out_index` checked non-null; writable per contract.
                            *out_index = idx as JInt;
                        }
                        JNI_OK
                    }
                    None => {
                        set_last_error(format!(
                            "cratonvm_field_index: no instance field \"{field_name}\" on class"
                        ));
                        JNI_ERR
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_field_index: panic");
        JNI_ERR
    })
}

/// `int32_t cratonvm_field_index_desc(CratonVm *vm, CratonClass cls,
///     const char *name, const char *descriptor, int32_t *out_index)`
///
/// Descriptor-disambiguated companion to [`cratonvm_field_index`]: resolve an
/// instance field by `name`, additionally requiring its JVM type `descriptor`
/// to match (`"I"`, `"Ljava/lang/String;"`, `"[J"`, …). This is what lets a
/// host address a **shadowed** super-class field that a subclass re-declares
/// with the same name — a plain name resolve always returns the most-derived
/// declaration, but passing the super-class field's descriptor walks past the
/// subclass shadow to the intended slot.
///
/// `descriptor` may be **null**, in which case this behaves exactly like
/// [`cratonvm_field_index`] (name-only; most-derived wins). Writes the slot to
/// `*out_index` and returns [`JNI_OK`], or [`JNI_ERR`] (last error set,
/// `*out_index` untouched) when the class is unloaded or has no instance field
/// matching both name and (when supplied) descriptor. The resolved index is
/// usable with [`cratonvm_get_field`] / [`cratonvm_set_field`].
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `cls` is a [`CratonClass`] the VM
/// handed out; `name` is a valid NUL-terminated C string; `descriptor` is null
/// or a valid NUL-terminated C string; `out_index`, when non-null, is writable.
#[no_mangle]
pub extern "C" fn cratonvm_field_index_desc(
    vm: *mut CratonVm,
    cls: CratonClass,
    name: *const c_char,
    descriptor: *const c_char,
    out_index: *mut JInt,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                // SAFETY: caller contract — `name` is a valid C string or null.
                let field_name = match cstr_or_err(name, "field name") {
                    Some(s) => s,
                    None => return JNI_ERR,
                };
                // `descriptor` is optional: null → name-only resolution.
                let desc: Option<&str> = if descriptor.is_null() {
                    None
                } else {
                    // SAFETY: caller contract — non-null `descriptor` is a valid C string.
                    match cstr_or_err(descriptor, "field descriptor") {
                        Some(s) => Some(s),
                        None => return JNI_ERR,
                    }
                };
                let class_id = ClassId::new(cls as u32);
                match h.vm.instance_field_index_desc(class_id, field_name, desc) {
                    Some(idx) => {
                        if !out_index.is_null() {
                            // SAFETY: `out_index` checked non-null; writable per contract.
                            *out_index = idx as JInt;
                        }
                        JNI_OK
                    }
                    None => {
                        match desc {
                            Some(d) => set_last_error(format!(
                                "cratonvm_field_index_desc: no instance field \"{field_name}\" with descriptor \"{d}\" on class"
                            )),
                            None => set_last_error(format!(
                                "cratonvm_field_index_desc: no instance field \"{field_name}\" on class"
                            )),
                        }
                        JNI_ERR
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_field_index_desc: panic");
        JNI_ERR
    })
}

/// `CratonValue cratonvm_get_field_by_name(CratonVm *vm, CratonRef obj, const char *name)`
///
/// Read the named instance field of `obj` (resolved against `obj`'s **runtime**
/// class via [`cratonvm_field_index`]) as a typed [`CratonValue`] — the
/// name-based companion to the index-based [`cratonvm_get_field`]. Returns
/// `tag == craton_tag::ERROR` (last error set) on a bad VM/handle or an unknown
/// field name.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `obj` is a live [`CratonRef`];
/// `name` is a valid NUL-terminated C string.
#[no_mangle]
pub extern "C" fn cratonvm_get_field_by_name(
    vm: *mut CratonVm,
    obj: CratonRef,
    name: *const c_char,
) -> CratonValue {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, CratonValue::error(), |h| {
                let oref = match resolve_handle(&h.vm.shared, obj) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_get_field_by_name: null object handle");
                        return CratonValue::error();
                    }
                };
                // SAFETY: caller contract — `name` is a valid C string or null.
                let field_name = match cstr_or_err(name, "field name") {
                    Some(s) => s,
                    None => return CratonValue::error(),
                };
                let class_id = h.vm.shared.mem.heap.class_id_of(oref);
                match h.vm.instance_field_index(class_id, field_name) {
                    Some(idx) => {
                        CratonValue::from_value(&h.vm.shared, h.vm.get_instance_field(oref, idx))
                    }
                    None => {
                        set_last_error(format!(
                            "cratonvm_get_field_by_name: no instance field \"{field_name}\""
                        ));
                        CratonValue::error()
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_get_field_by_name: panic");
        CratonValue::error()
    })
}

/// `int32_t cratonvm_set_field(CratonVm *vm, CratonRef obj, int32_t index, CratonValue value)`
///
/// Write `value` into instance-field slot `index` of `obj` — the write-back
/// companion to [`cratonvm_get_field`]. The write is **GC-barrier correct**
/// (the VM applies the SATB pre-barrier + post write-barrier exactly as the
/// interpreter's `putfield` does). Returns [`JNI_OK`], or [`JNI_ERR`] (last
/// error set) on a bad VM/handle or an out-of-range `index`. No coercion is
/// performed — the caller is responsible for the `CratonValue` tag matching the
/// field's declared type.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `obj` is a live [`CratonRef`].
#[no_mangle]
pub extern "C" fn cratonvm_set_field(
    vm: *mut CratonVm,
    obj: CratonRef,
    index: JInt,
    value: CratonValue,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                let oref = match resolve_handle(&h.vm.shared, obj) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_set_field: null object handle");
                        return JNI_ERR;
                    }
                };
                let class_id = h.vm.shared.mem.heap.class_id_of(oref);
                let nfields = h.vm.instance_field_count(class_id);
                if index < 0 || (index as usize) >= nfields {
                    set_last_error(format!(
                        "cratonvm_set_field: index {index} out of range for {nfields} field(s)"
                    ));
                    return JNI_ERR;
                }
                let value = match decode_craton_value("cratonvm_set_field", &h.vm.shared, value) {
                    Some(value) => value,
                    None => return JNI_ERR,
                };
                h.vm.set_instance_field(oref, index as usize, value);
                JNI_OK
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_set_field: panic");
        JNI_ERR
    })
}

/// `int32_t cratonvm_set_field_by_name(CratonVm *vm, CratonRef obj, const char *name, CratonValue value)`
///
/// Write `value` into the named instance field of `obj` (resolved against
/// `obj`'s **runtime** class) — the name-based companion to [`cratonvm_set_field`],
/// GC-barrier correct. Returns [`JNI_OK`], or [`JNI_ERR`] (last error set) on a
/// bad VM/handle or an unknown field name.
///
/// # Safety
/// `vm` is a handle from [`cratonvm_create`]; `obj` is a live [`CratonRef`];
/// `name` is a valid NUL-terminated C string.
#[no_mangle]
pub extern "C" fn cratonvm_set_field_by_name(
    vm: *mut CratonVm,
    obj: CratonRef,
    name: *const c_char,
    value: CratonValue,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        clear_last_error();
        // SAFETY: `vm` per caller contract.
        unsafe {
            with_vm(vm, JNI_ERR, |h| {
                let oref = match resolve_handle(&h.vm.shared, obj) {
                    Some(r) => r,
                    None => {
                        set_last_error("cratonvm_set_field_by_name: null object handle");
                        return JNI_ERR;
                    }
                };
                // SAFETY: caller contract — `name` is a valid C string or null.
                let field_name = match cstr_or_err(name, "field name") {
                    Some(s) => s,
                    None => return JNI_ERR,
                };
                let class_id = h.vm.shared.mem.heap.class_id_of(oref);
                match h.vm.instance_field_index(class_id, field_name) {
                    Some(idx) => {
                        let value = match decode_craton_value(
                            "cratonvm_set_field_by_name",
                            &h.vm.shared,
                            value,
                        ) {
                            Some(value) => value,
                            None => return JNI_ERR,
                        };
                        h.vm.set_instance_field(oref, idx, value);
                        JNI_OK
                    }
                    None => {
                        set_last_error(format!(
                            "cratonvm_set_field_by_name: no instance field \"{field_name}\""
                        ));
                        JNI_ERR
                    }
                }
            })
        }
    }))
    .unwrap_or_else(|_| {
        set_last_error("cratonvm_set_field_by_name: panic");
        JNI_ERR
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(unexpected_cfgs)] // `flat_api_live_vm` is an opt-in cfg cargo can't learn from Cargo.toml
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
    fn default_init_args_rejects_unsupported_version() {
        let mut args = JavaVMInitArgs {
            version: 0x0001_0009,
            n_options: 0,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 0,
        };
        let rc = JNI_GetDefaultJavaVMInitArgs(&mut args as *mut _ as *mut c_void);
        assert_eq!(rc, JNI_EVERSION);
        assert_eq!(args.version, JNI_VERSION);
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
    fn destroy_created_vm_without_vm_is_noop_ok() {
        // The `DestroyJavaVM` teardown is a no-op success when no Invocation-API
        // VM is parked (the default test run never bootstraps one — the flat API
        // does not touch `CREATED_VM`). This also pins idempotent double-destroy.
        assert_eq!(destroy_created_vm(), JNI_OK);
        assert_eq!(destroy_created_vm(), JNI_OK);
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

    // ── JDK-mode validation fixtures ──────────────────────────────────────
    //
    // `config_from_args` now validates that the selected class library really
    // exists (see `validate_jdk_mode`). That makes it host-dependent unless
    // the test points it at a JDK, so these helpers synthesise a minimal one
    // — the same trick `vm/src/config.rs`'s detection tests use. Without them
    // the suite would pass or fail depending on whether the machine running it
    // happens to have a JDK installed, which is exactly the non-determinism
    // this whole change exists to remove.

    /// `JAVA_HOME` is process-wide; serialise.
    ///
    /// `CRATONVM_JAVA_HOME` no longer needs this — it is a *declared* flag and
    /// is now overridden per-thread through
    /// [`cratonvm_types::flags::with_thread_overrides`] — but `JAVA_HOME` is
    /// not declared, so it keeps `std::env`'s live-read semantics and still
    /// has to be stashed and restored under a lock.
    fn jdk_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Set an **undeclared** environment variable for the duration of `f`.
    ///
    /// Declared `CRATONVM_*` flags must not go through here: they are served
    /// from the frozen [`cratonvm_types::flags::flags`] snapshot, so mutating
    /// `environ` would change nothing that the code under test reads. Use
    /// `flags::with_thread_overrides` for those.
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        debug_assert!(
            !key.starts_with("CRATONVM_"),
            "{key} looks like a declared flag; use flags::with_thread_overrides"
        );
        let prev = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let result = f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        result
    }

    /// A scratch directory removed on drop. With `jmods = true` it looks
    /// enough like a JDK 9+ root for `require_real_jdk`; with `jmods = false`
    /// it is a bare directory that `resolve_java_home` accepts but
    /// `detect_real_jdk` must reject.
    struct ScratchJdk(std::path::PathBuf);

    impl ScratchJdk {
        fn new(tag: &str, jmods: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "cratonvm-libjni-jdk-{}-{}-{:?}",
                tag,
                std::process::id(),
                std::thread::current().id(),
            ));
            let _ = std::fs::remove_dir_all(&root);
            if jmods {
                let dir = root.join("jmods");
                std::fs::create_dir_all(&dir).expect("create scratch jmods dir");
                std::fs::write(dir.join("java.base.jmod"), b"JM\x01\x00")
                    .expect("write scratch jmod");
            } else {
                std::fs::create_dir_all(&root).expect("create scratch dir");
            }
            Self(root)
        }
        fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 temp path")
        }
    }

    impl Drop for ScratchJdk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run `f` with the JDK search pointed at `root`.
    ///
    /// `CRATONVM_JAVA_HOME` is a declared flag, so it is overridden on the
    /// [`cratonvm_types::flags`] snapshot for this thread rather than by
    /// `set_var`. `resolve_java_home` reads it through
    /// `flags::runtime_var`, which means an `environ` write would have been
    /// ignored by every test but whichever one happened to run first and latch
    /// the snapshot — the defect these fixtures existed to avoid in the first
    /// place. `JAVA_HOME` is *not* declared and keeps live-read semantics, so
    /// it still goes through `with_env`.
    fn with_java_home<R>(root: &str, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JAVA_HOME", Some(root))], || {
            with_env("JAVA_HOME", None, f)
        })
    }

    /// Run `f` with the process pointed at a synthesised real JDK.
    fn with_fake_jdk<R>(tag: &str, f: impl FnOnce() -> R) -> R {
        let _guard = jdk_env_lock();
        let jdk = ScratchJdk::new(tag, true);
        with_java_home(jdk.path(), f)
    }

    /// Run `f` with no usable JDK.
    ///
    /// Points `CRATONVM_JAVA_HOME` at an *empty real directory* rather than
    /// emptying `PATH`: on Windows `CreateProcess` can still resolve a system
    /// `java` with an empty `PATH`, which would make the test flaky on
    /// developer machines. Same rationale as
    /// `config.rs::launcher_default_never_falls_back_to_synthetic_when_no_jdk`.
    fn with_no_jdk<R>(tag: &str, f: impl FnOnce() -> R) -> R {
        let _guard = jdk_env_lock();
        let empty = ScratchJdk::new(tag, false);
        with_java_home(empty.path(), f)
    }

    // `JavaVMInitArgs::ignoreUnrecognized` is a JNI `jboolean` (`u8`), not a
    // `jint`; all three call sites pass an untyped literal, so taking `u8`
    // here is exact rather than a lossy conversion at the struct literal.
    fn args_with(opts: &mut [JavaVMOption], ignore_unrecognized: u8) -> JavaVMInitArgs {
        JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized,
        }
    }

    #[test]
    fn config_from_args_rejects_unsupported_jni_version() {
        let args = JavaVMInitArgs {
            version: 0x0001_0009,
            n_options: 0,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 1,
        };
        let result = unsafe { config_from_args(&args) };
        assert_eq!(
            result.err(),
            Some(InitArgsError::UnsupportedVersion(0x0001_0009))
        );
    }

    #[test]
    fn config_from_args_honors_ignore_unrecognized() {
        let unknown = std::ffi::CString::new("-XX:NoSuchCratonOption").unwrap();
        let mut opts = [JavaVMOption {
            option_string: unknown.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        }];
        let mut args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };

        let result = unsafe { config_from_args(&args) };
        assert_eq!(
            result.err(),
            Some(InitArgsError::UnrecognizedOption(
                "-XX:NoSuchCratonOption".to_string()
            ))
        );

        // With `ignoreUnrecognized` the unknown option is dropped — but the
        // config still has to name a class library that exists, so this half
        // runs against a synthesised JDK.
        args.ignore_unrecognized = 1;
        with_fake_jdk("ignore-unrecognized", || {
            assert!(unsafe { config_from_args(&args) }.is_ok());
        });
    }

    // ── JDK-mode validation on the C-ABI entry point ──────────────────────
    //
    // `jdk-mode-determinism.md` §6.4: this path called
    // `VmConfig::with_host_jdk_default()` and performed *no* validation, so an
    // embedder could boot into a mode whose backing library is not present and
    // only find out much later, as an unexplained `NoClassDefFoundError`.

    #[test]
    fn config_from_args_defaults_to_real_jdk() {
        with_fake_jdk("default-mode", || {
            let args = JavaVMInitArgs {
                version: JNI_VERSION,
                n_options: 0,
                options: std::ptr::null_mut(),
                ignore_unrecognized: 1,
            };
            let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
            assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Real);
            assert_eq!(
                cfg.jdk_mode(),
                cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE,
                "the embedding entry point must agree with the launcher default"
            );
        });
    }

    #[test]
    fn config_from_args_pins_the_validated_java_home() {
        // The point of pinning: boot-classpath discovery inside the VM must
        // resolve the installation that was just validated, not re-run the
        // environment probe and possibly land somewhere else.
        with_fake_jdk("pin-home", || {
            let args = JavaVMInitArgs {
                version: JNI_VERSION,
                n_options: 0,
                options: std::ptr::null_mut(),
                ignore_unrecognized: 1,
            };
            let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
            assert!(
                cfg.java_home.is_some(),
                "the resolved JDK root must be pinned onto the config"
            );
        });
    }

    #[test]
    fn config_from_args_fails_loudly_when_no_jdk_is_available() {
        with_no_jdk("no-jdk", || {
            let args = JavaVMInitArgs {
                version: JNI_VERSION,
                n_options: 0,
                options: std::ptr::null_mut(),
                ignore_unrecognized: 1,
            };
            let err = unsafe { config_from_args(&args) }
                .expect_err("real-JDK mode with no JDK must not boot an empty VM");
            match err {
                InitArgsError::JdkModeUnavailable(msg) => {
                    assert!(msg.contains("no usable JDK was found"), "{msg}");
                    // The message must be actionable, not just a verdict.
                    assert!(msg.contains("JAVA_HOME"), "{msg}");
                    assert!(msg.contains("--java-home"), "{msg}");
                }
                other => panic!("expected JdkModeUnavailable, got {other:?}"),
            }
        });
    }

    #[test]
    fn config_from_args_rejects_both_mode_flags() {
        let synth = std::ffi::CString::new("--synthetic-jdk").unwrap();
        let real = std::ffi::CString::new("--real-jdk").unwrap();
        let mut opts = [
            JavaVMOption {
                option_string: synth.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            },
            JavaVMOption {
                option_string: real.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            },
        ];
        // `ignoreUnrecognized` must NOT soften this: both flags were
        // recognized, they just contradict each other.
        let args = args_with(&mut opts, 1);
        let err = unsafe { config_from_args(&args) }.expect_err("both flags must be rejected");
        match err {
            InitArgsError::InvalidOption(msg) => {
                assert!(msg.contains("mutually exclusive"), "{msg}")
            }
            other => panic!("expected InvalidOption, got {other:?}"),
        }
    }

    #[test]
    fn config_from_args_honours_the_synthetic_flag_subject_to_the_cargo_feature() {
        let synth = std::ffi::CString::new("--synthetic-jdk").unwrap();
        let mut opts = [JavaVMOption {
            option_string: synth.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        }];
        let args = args_with(&mut opts, 1);
        // Deliberately run with NO usable JDK: synthetic mode must not consult
        // the host at all. The outcome depends only on whether the stubs were
        // compiled in.
        with_no_jdk("synthetic-flag", || {
            let result = unsafe { config_from_args(&args) };
            if cratonvm_vm::config::SYNTHETIC_JDK_COMPILED_IN {
                let cfg = result.expect("synthetic mode is available in this build");
                assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Synthetic);
            } else {
                match result.expect_err("synthetic mode without the feature must be rejected") {
                    InitArgsError::JdkModeUnavailable(msg) => {
                        assert!(msg.contains("synthetic-jdk"), "{msg}");
                        assert!(
                            msg.contains("Cargo feature"),
                            "the error must name the fix: {msg}"
                        );
                    }
                    other => panic!("expected JdkModeUnavailable, got {other:?}"),
                }
            }
        });
    }

    #[test]
    fn config_from_args_honours_the_real_flag() {
        let real = std::ffi::CString::new("--real-jdk").unwrap();
        let mut opts = [JavaVMOption {
            option_string: real.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        }];
        let args = args_with(&mut opts, 0);
        with_fake_jdk("real-flag", || {
            let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
            assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Real);
        });
    }

    // ── Compatibility mode on the C-ABI entry points ──────────────────────
    //
    // `docs/feature-designs/jdk-only-mode.md` §6/§10 and `docs/EMBEDDING.md`
    // ("Choosing a compatibility mode"). Four resolution rules are deliberate
    // and each is pinned below, because each one is a plausible-looking
    // "simplification" away from being wrong in a way no compiler catches:
    // the default is reached by doing nothing; an unknown ABI integer is
    // rejected rather than clamped; an explicit COMPATIBLE plus `--jdk-only`
    // is a contradiction rather than a precedence rule; and `--jdk-only` does
    // not rewrite the JDK mode.

    fn jvm_option(s: &std::ffi::CString) -> JavaVMOption {
        JavaVMOption {
            option_string: s.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        }
    }

    /// The ABI numbers are published and append-only: a compiled host carries
    /// them in its `.text`, so this test failing means every deployed binary
    /// that linked an older header is now wrong.
    #[test]
    fn compatibility_abi_values_are_published_and_append_only() {
        assert_eq!(craton_compatibility::COMPATIBLE, 0);
        assert_eq!(craton_compatibility::JDK_ONLY, 1);
        assert_eq!(
            CRATONVM_COMPATIBILITY_COMPATIBLE,
            craton_compatibility::COMPATIBLE
        );
        assert_eq!(
            CRATONVM_COMPATIBILITY_JDK_ONLY,
            craton_compatibility::JDK_ONLY
        );

        for (abi, mode) in [
            (
                CRATONVM_COMPATIBILITY_COMPATIBLE,
                CompatibilityMode::Compatible,
            ),
            (CRATONVM_COMPATIBILITY_JDK_ONLY, CompatibilityMode::JdkOnly),
        ] {
            assert_eq!(compatibility_mode_from_abi(abi), Ok(mode));
            assert_eq!(compatibility_mode_to_abi(mode), abi);
        }

        // `-1` is `cratonvm_compatibility_mode`'s bad-handle sentinel; it must
        // never become a mode value, which is why the encoding grows upwards.
        assert_ne!(CRATONVM_COMPATIBILITY_COMPATIBLE, -1);
        assert_ne!(CRATONVM_COMPATIBILITY_JDK_ONLY, -1);
    }

    /// An unknown ABI integer is rejected, **never clamped to COMPATIBLE**: a
    /// host compiled against a newer header and run against an older library
    /// must be told, not silently handed the loose policy it opted out of.
    #[test]
    fn unknown_compatibility_mode_is_rejected_not_clamped() {
        for bogus in [2, -1, 7, JInt::MAX, JInt::MIN] {
            match compatibility_mode_from_abi(bogus) {
                Err(InitArgsError::UnknownCompatibilityMode(v)) => assert_eq!(v, bogus),
                other => panic!("expected UnknownCompatibilityMode({bogus}), got {other:?}"),
            }
        }
        // The exact wording is quoted in docs/EMBEDDING.md.
        assert_eq!(
            InitArgsError::UnknownCompatibilityMode(2).to_string(),
            "unknown compatibility mode 2: expected 0 (compatible) or 1 (jdk-only)"
        );
    }

    /// The default is the **launcher's**, because this crate's base config is
    /// `with_host_jdk_default()`. Unlike `JdkMode`, the two entry-point
    /// defaults agree — and that agreement is the point, not an accident.
    #[test]
    fn default_compatibility_mode_is_the_launcher_default_and_is_compatible() {
        assert_eq!(
            DEFAULT_COMPATIBILITY_MODE,
            cratonvm_vm::config::LAUNCHER_DEFAULT_COMPATIBILITY_MODE
        );
        assert_eq!(DEFAULT_COMPATIBILITY_MODE, CompatibilityMode::Compatible);

        with_fake_jdk("default-compat", || {
            let args = JavaVMInitArgs {
                version: JNI_VERSION,
                n_options: 0,
                options: std::ptr::null_mut(),
                ignore_unrecognized: 1,
            };
            let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
            assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
            assert!(!cfg.is_jdk_only());
        });
    }

    /// The whole resolution matrix, without touching the host: doing nothing
    /// is compatible, either explicit route is strict, the two agreeing is
    /// fine, and only the explicit contradiction fails.
    #[test]
    fn apply_compatibility_resolution_matrix() {
        let base = VmConfig::with_host_jdk_default();

        let cases = [
            (None, false, CompatibilityMode::Compatible),
            (None, true, CompatibilityMode::JdkOnly),
            (
                Some(CompatibilityMode::Compatible),
                false,
                CompatibilityMode::Compatible,
            ),
            (
                Some(CompatibilityMode::JdkOnly),
                false,
                CompatibilityMode::JdkOnly,
            ),
            (
                Some(CompatibilityMode::JdkOnly),
                true,
                CompatibilityMode::JdkOnly,
            ),
        ];
        for (requested, jdk_only_option, expected) in cases {
            let cfg = apply_compatibility(base.clone(), requested, jdk_only_option)
                .unwrap_or_else(|e| panic!("{requested:?} + {jdk_only_option} rejected: {e}"));
            assert_eq!(cfg.compatibility_mode, expected);
            // Never rewritten: the base config's JDK mode survives untouched,
            // so a later `--synthetic-jdk` conflict is still detectable.
            assert_eq!(cfg.jdk_mode(), base.jdk_mode());
        }

        // Explicit COMPATIBLE + `--jdk-only` is a contradiction, not a
        // precedence rule: both are explicit statements of policy, and picking
        // a winner would run a host under a policy neither half asked for.
        match apply_compatibility(base, Some(CompatibilityMode::Compatible), true) {
            Err(InitArgsError::InvalidOption(msg)) => {
                assert!(msg.contains("--jdk-only"), "{msg}");
                assert!(msg.contains("contradict"), "{msg}");
                assert!(msg.contains("CRATONVM_COMPATIBILITY_JDK_ONLY"), "{msg}");
            }
            other => panic!("expected InvalidOption contradiction, got {other:?}"),
        }
    }

    /// The option-string route, spelled exactly as the launcher flag — the
    /// only route available to `JNI_CreateJavaVM`, which takes nothing but a
    /// `JavaVMInitArgs`.
    #[test]
    fn jdk_only_option_string_selects_strict_without_rewriting_the_jdk_mode() {
        let jdk_only = std::ffi::CString::new("--jdk-only").unwrap();
        let mut opts = [jvm_option(&jdk_only)];
        // `ignoreUnrecognized = 0`: the flag must be *recognised*, not tolerated.
        let args = args_with(&mut opts, 0);
        with_fake_jdk("jdk-only-option", || {
            let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
            assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
            assert!(cfg.is_jdk_only());
            // The base config was already real; nothing forced it, so the
            // `--synthetic-jdk` conflict below stays detectable.
            assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Real);
            assert!(cfg.execution_policy().is_jdk_only());
            assert!(cfg.execution_policy().real_jdk);
        });
    }

    /// The typed argument and the option string agreeing is not an error.
    #[test]
    fn explicit_jdk_only_argument_agrees_with_the_option_string() {
        let jdk_only = std::ffi::CString::new("--jdk-only").unwrap();
        let mut opts = [jvm_option(&jdk_only)];
        let args = args_with(&mut opts, 0);
        with_fake_jdk("jdk-only-both", || {
            let cfg = unsafe {
                config_from_args_with_compatibility(&args, Some(CompatibilityMode::JdkOnly))
            }
            .expect("the two explicit statements agree");
            assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
        });
    }

    /// …and disagreeing is rejected at the entry point, with no host access
    /// needed to reach the verdict.
    #[test]
    fn explicit_compatible_plus_jdk_only_option_is_rejected() {
        let jdk_only = std::ffi::CString::new("--jdk-only").unwrap();
        let mut opts = [jvm_option(&jdk_only)];
        // `ignoreUnrecognized` must NOT soften this: `--jdk-only` was
        // recognised, it just contradicts the typed argument.
        let args = args_with(&mut opts, 1);
        let err = unsafe {
            config_from_args_with_compatibility(&args, Some(CompatibilityMode::Compatible))
        }
        .expect_err("explicit compatible + --jdk-only must be rejected");
        match err {
            InitArgsError::InvalidOption(msg) => assert!(msg.contains("contradict"), "{msg}"),
            other => panic!("expected InvalidOption, got {other:?}"),
        }
    }

    /// The conflict verdict is `VmConfig::validate_compatibility`'s, carried
    /// **byte for byte**. The rule is called here, never re-derived — so the C
    /// ABI and the `cratonvm` launcher can never drift into reporting the same
    /// conflict in different words.
    #[test]
    fn jdk_only_plus_synthetic_reports_validate_compatibility_verbatim() {
        let expected = VmConfig::with_host_jdk_default()
            .with_jdk_mode(cratonvm_vm::config::JdkMode::Synthetic)
            .with_compatibility_mode(CompatibilityMode::JdkOnly)
            .validate_compatibility()
            .expect_err("jdk-only + synthetic-jdk is incoherent")
            .to_string();
        assert!(
            expected.starts_with("invalid configuration: "),
            "the flat API's last error is documented as \
             \"<entry_point>: invalid configuration: …\": {expected}"
        );

        let jdk_only = std::ffi::CString::new("--jdk-only").unwrap();
        let synthetic = std::ffi::CString::new("--synthetic-jdk").unwrap();
        let mut opts = [jvm_option(&jdk_only), jvm_option(&synthetic)];
        let args = args_with(&mut opts, 1);
        let err = unsafe { config_from_args(&args) }
            .expect_err("jdk-only + synthetic-jdk must be rejected");
        match &err {
            InitArgsError::InvalidCompatibility(msg) => assert_eq!(*msg, expected),
            other => panic!("expected InvalidCompatibility, got {other:?}"),
        }
        // The `Display` is the message unchanged, so the last-error text a C
        // host reads is exactly `"<entry_point>: " + expected`.
        assert_eq!(err.to_string(), expected);
    }

    /// Validation order is normative: `--jdk-only --synthetic-jdk` is wrong on
    /// a machine with a perfect JDK and equally wrong on one with none, so the
    /// verdict must be a property of the *request*. Running the availability
    /// check first would send the operator off to install a JDK to fix a flag
    /// conflict.
    #[test]
    fn compatibility_is_validated_before_jdk_availability() {
        let jdk_only = std::ffi::CString::new("--jdk-only").unwrap();
        let synthetic = std::ffi::CString::new("--synthetic-jdk").unwrap();
        let mut opts = [jvm_option(&jdk_only), jvm_option(&synthetic)];
        let args = args_with(&mut opts, 1);
        with_no_jdk("compat-before-jdk", || {
            match unsafe { config_from_args(&args) } {
                Err(InitArgsError::InvalidCompatibility(msg)) => {
                    assert!(msg.contains("--jdk-only"), "{msg}");
                    assert!(msg.contains("--synthetic-jdk"), "{msg}");
                }
                other => panic!(
                    "the incoherent request must be rejected on its own terms, \
                     not blamed on the host: {other:?}"
                ),
            }
        });
    }

    /// The capability probe needs no VM — that is its whole reason to exist,
    /// since the alternative is learning the answer from a failed create.
    #[test]
    fn compatibility_mode_supported_probes_without_a_vm() {
        assert_eq!(
            cratonvm_compatibility_mode_supported(CRATONVM_COMPATIBILITY_COMPATIBLE),
            1
        );
        assert_eq!(
            cratonvm_compatibility_mode_supported(CRATONVM_COMPATIBILITY_JDK_ONLY),
            1
        );
        // `-1` = "not a CRATONVM_COMPATIBILITY_* value at all"; `0` stays
        // reserved for known-but-unhonourable and is never returned today.
        for bogus in [2, -1, 99] {
            assert_eq!(cratonvm_compatibility_mode_supported(bogus), -1);
        }
    }

    #[test]
    fn compatibility_mode_null_handle_returns_minus_one() {
        clear_last_error();
        assert_eq!(cratonvm_compatibility_mode(std::ptr::null_mut()), -1);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn create_with_compatibility_rejects_an_unknown_mode_before_touching_the_vm() {
        clear_last_error();
        let vm = cratonvm_create_with_compatibility(std::ptr::null(), 2);
        assert!(vm.is_null(), "an unknown mode must not create a VM");
        let msg = last_error_string().expect("last error is set");
        assert!(
            msg.starts_with("cratonvm_create_with_compatibility: "),
            "the last error must name the entry point the host called: {msg}"
        );
        assert!(msg.contains("unknown compatibility mode 2"), "{msg}");
    }

    // -- Layer 2 flat C API ------------------------------------------------
    //
    // These exercise the FFI surface without the heavy VM bootstrap where
    // possible (null-handle handling, last-error round-trip, value
    // marshalling). One opt-in round-trip test that actually creates a VM is
    // gated on `--cfg flat_api_live_vm` so the default `cargo test` stays
    // fast and JDK-independent (mirroring the increment-1 convention of not
    // booting a VM in the default unit run).

    use std::ffi::CString;

    /// Read the calling thread's last-error C string back into a Rust `String`
    /// (helper for assertions). Returns `None` when no error is pending.
    fn last_error_string() -> Option<String> {
        let p = cratonvm_last_error(std::ptr::null_mut());
        if p.is_null() {
            None
        } else {
            // SAFETY: `p` is the library-owned C string for this thread.
            Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
        }
    }

    #[test]
    fn craton_value_round_trips_primitive_tags() {
        // int / long / float / double survive a `to_value → from_value` round
        // trip with identical bit content. These tags are independent of the
        // VM (no handle-table access), so the conversion is exercised through a
        // never-dereferenced `&SharedVm` (the primitive arms ignore it). The
        // OBJECT tag, which *does* go through the per-VM handle table, is
        // covered end-to-end by `flat_api_live_round_trip`.
        let cases = [
            CratonValue {
                tag: craton_tag::INT,
                payload: (-42i32) as u32 as u64,
            },
            CratonValue {
                tag: craton_tag::LONG,
                payload: 0x0123_4567_89ab_cdef,
            },
            CratonValue {
                tag: craton_tag::FLOAT,
                payload: 1.5f32.to_bits() as u64,
            },
            CratonValue {
                tag: craton_tag::DOUBLE,
                payload: (-2.25f64).to_bits(),
            },
        ];
        // SAFETY: the primitive `to_value`/`from_value` arms never read through
        // this reference; a dangling pointer is only formed, never dereferenced.
        let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
        for c in cases {
            let v = c.to_value(shared).expect("primitive tag should decode");
            let back = CratonValue::from_value(shared, v);
            assert_eq!(back.tag, c.tag, "tag changed for {:?}", c.tag);
            assert_eq!(back.payload, c.payload, "payload changed for tag {}", c.tag);
        }
    }

    #[test]
    fn craton_value_rejects_unknown_input_tags() {
        clear_last_error();
        // SAFETY: the unknown-tag path rejects before reading through `shared`.
        let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
        let bad = [CratonValue {
            tag: 99,
            payload: 0,
        }];
        assert!(decode_craton_args("test_api", shared, &bad).is_none());
        let err = last_error_string().expect("invalid tag should set last error");
        assert!(
            err.contains("unknown CratonValue tag 99"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn craton_value_rejects_stale_object_tokens() {
        clear_last_error();
        // SAFETY: this path only uses the address of `shared` as a table key.
        let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
        let stale = CratonValue {
            tag: craton_tag::OBJECT,
            payload: 0xfeed,
        };
        assert!(decode_craton_value("test_api", shared, stale).is_none());
        let err = last_error_string().expect("stale object token should set last error");
        assert!(
            err.contains("stale or unknown CratonRef object token 0xfeed"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn object_handle_dedup_uses_gc_updated_global_refs() {
        let shared = SharedVm::new(VmConfig::default());
        // SAFETY: these aligned fake refs are only stored in JniGlobalRefs and
        // compared by address; the test never dereferences them as heap objects.
        let old = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        let moved = unsafe { ObjectRef::from_raw(0x2000 as *mut u8) };
        let reused_old_address = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };

        let first = register_handle(&shared, Some(old));
        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old.as_ptr() as usize, moved.as_ptr() as usize);
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(&pointer_map);

        let after_move = register_handle(&shared, Some(moved));
        assert_eq!(
            after_move, first,
            "moved object should keep its existing token"
        );

        let new_at_old_address = register_handle(&shared, Some(reused_old_address));
        assert_ne!(
            new_at_old_address, first,
            "a new object at the stale pre-GC address must not inherit the token"
        );

        drop_handle_table(&shared);
    }

    #[test]
    fn release_handle_balances_deduped_object_tokens() {
        let shared = SharedVm::new(VmConfig::default());
        // SAFETY: this aligned fake ref is stored in JniGlobalRefs and compared
        // by address; the test never dereferences it as a heap object.
        let obj = unsafe { ObjectRef::from_raw(0x3000 as *mut u8) };

        let first = register_handle(&shared, Some(obj));
        let second = register_handle(&shared, Some(obj));
        assert_eq!(second, first, "same object should dedup to one token");

        assert!(release_handle(&shared, first));
        assert!(
            resolve_handle(&shared, first).is_some(),
            "one release should leave the deduped token live"
        );

        assert!(release_handle(&shared, second));
        assert!(
            resolve_handle(&shared, first).is_none(),
            "balanced releases should remove the token"
        );
        assert!(
            !release_handle(&shared, first),
            "fully released tokens must be rejected as stale"
        );

        drop_handle_table(&shared);
    }

    #[test]
    fn null_object_handle_resolves_to_null_without_vm() {
        // The reserved null token (`0`) must short-circuit before any handle
        // table or `SharedVm` access, so a dangling reference is safe here.
        // SAFETY: `resolve_handle` returns on `h == 0` before reading `shared`.
        let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
        assert!(resolve_handle(shared, 0).is_none());
        // `register_handle(None)` likewise returns the null token without access.
        assert_eq!(register_handle(shared, None), 0);
        assert!(release_handle(shared, 0));
    }

    #[test]
    fn null_handle_sets_last_error_and_returns_err() {
        clear_last_error();
        // load_class with a null VM handle → JNI_ERR + last error.
        let name = CString::new("java/lang/Object").unwrap();
        let mut out: CratonClass = u64::MAX;
        let rc = cratonvm_load_class(std::ptr::null_mut(), name.as_ptr(), &mut out);
        assert_eq!(rc, JNI_ERR);
        assert_eq!(out, u64::MAX, "out_class must be untouched on error");
        let err = last_error_string().expect("last error should be set for null handle");
        assert!(err.contains("null"), "unexpected error text: {err}");
    }

    #[test]
    fn invoke_static_null_handle_returns_error_value() {
        clear_last_error();
        let class = CString::new("java/lang/System").unwrap();
        let method = CString::new("gc").unwrap();
        let sig = CString::new("()V").unwrap();
        let r = cratonvm_invoke_static(
            std::ptr::null_mut(),
            class.as_ptr(),
            method.as_ptr(),
            sig.as_ptr(),
            std::ptr::null(),
            0,
        );
        assert_eq!(r.tag, craton_tag::ERROR);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn new_string_null_handle_returns_null_and_sets_error() {
        clear_last_error();
        let s = CString::new("hello").unwrap();
        let h = cratonvm_new_string(std::ptr::null_mut(), s.as_ptr());
        assert_eq!(h, 0);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn string_utf8_null_handle_returns_null_and_sets_error() {
        clear_last_error();
        let p = cratonvm_string_utf8(std::ptr::null_mut(), 0);
        assert!(p.is_null());
        assert!(last_error_string().is_some());
    }

    #[test]
    fn free_string_null_is_noop() {
        // Must not panic / segfault.
        cratonvm_free_string(std::ptr::null_mut());
    }

    #[test]
    fn invoke_virtual_null_handle_returns_error_value() {
        clear_last_error();
        let method = CString::new("length").unwrap();
        let sig = CString::new("()I").unwrap();
        let r = cratonvm_invoke_virtual(
            std::ptr::null_mut(),
            0,
            method.as_ptr(),
            sig.as_ptr(),
            std::ptr::null(),
            0,
        );
        assert_eq!(r.tag, craton_tag::ERROR);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn invoke_virtual_null_receiver_with_live_vm_unneeded_is_error() {
        // Even with a (here null) VM handle, a 0 receiver must be rejected
        // before any VM access — the null-VM branch fires first, but this
        // pins that a 0 receiver is never dereferenced.
        clear_last_error();
        let method = CString::new("length").unwrap();
        let sig = CString::new("()I").unwrap();
        let r = cratonvm_invoke_virtual(
            std::ptr::null_mut(),
            0,
            method.as_ptr(),
            sig.as_ptr(),
            std::ptr::null(),
            0,
        );
        assert_eq!(r.tag, craton_tag::ERROR);
    }

    #[test]
    fn object_class_null_handle_returns_err() {
        clear_last_error();
        let mut out: CratonClass = u64::MAX;
        let rc = cratonvm_object_class(std::ptr::null_mut(), 0, &mut out);
        assert_eq!(rc, JNI_ERR);
        assert_eq!(out, u64::MAX, "out_class must be untouched on error");
        assert!(last_error_string().is_some());
    }

    #[test]
    fn class_name_null_handle_returns_null_and_sets_error() {
        clear_last_error();
        let p = cratonvm_class_name(std::ptr::null_mut(), 0);
        assert!(p.is_null());
        assert!(last_error_string().is_some());
    }

    #[test]
    fn field_count_null_handle_returns_minus_one() {
        clear_last_error();
        assert_eq!(cratonvm_field_count(std::ptr::null_mut(), 0), -1);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn get_field_null_handle_returns_error_value() {
        clear_last_error();
        let r = cratonvm_get_field(std::ptr::null_mut(), 0, 0);
        assert_eq!(r.tag, craton_tag::ERROR);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn field_index_null_handle_returns_err() {
        clear_last_error();
        let name = CString::new("h").unwrap();
        let mut out: JInt = -999;
        let rc = cratonvm_field_index(std::ptr::null_mut(), 0, name.as_ptr(), &mut out);
        assert_eq!(rc, JNI_ERR);
        assert_eq!(out, -999, "out_index must be untouched on error");
        assert!(last_error_string().is_some());
    }

    #[test]
    fn field_index_desc_null_handle_returns_err() {
        clear_last_error();
        let name = CString::new("h").unwrap();
        let desc = CString::new("I").unwrap();
        let mut out: JInt = -999;
        // Both with and without a descriptor, a null VM handle must short-circuit
        // to JNI_ERR before any VM access, leaving out_index untouched.
        let rc = cratonvm_field_index_desc(
            std::ptr::null_mut(),
            0,
            name.as_ptr(),
            desc.as_ptr(),
            &mut out,
        );
        assert_eq!(rc, JNI_ERR);
        assert_eq!(out, -999, "out_index must be untouched on error");
        assert!(last_error_string().is_some());
        // Null descriptor (name-only) path, same null-VM rejection.
        let rc2 = cratonvm_field_index_desc(
            std::ptr::null_mut(),
            0,
            name.as_ptr(),
            std::ptr::null(),
            &mut out,
        );
        assert_eq!(rc2, JNI_ERR);
    }

    #[test]
    fn get_field_by_name_null_handle_returns_error_value() {
        clear_last_error();
        let name = CString::new("h").unwrap();
        let r = cratonvm_get_field_by_name(std::ptr::null_mut(), 0, name.as_ptr());
        assert_eq!(r.tag, craton_tag::ERROR);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn set_field_null_handle_returns_err() {
        clear_last_error();
        let rc = cratonvm_set_field(
            std::ptr::null_mut(),
            0,
            0,
            CratonValue {
                tag: craton_tag::INT,
                payload: 1,
            },
        );
        assert_eq!(rc, JNI_ERR);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn set_field_by_name_null_handle_returns_err() {
        clear_last_error();
        let name = CString::new("h").unwrap();
        let rc = cratonvm_set_field_by_name(
            std::ptr::null_mut(),
            0,
            name.as_ptr(),
            CratonValue {
                tag: craton_tag::INT,
                payload: 1,
            },
        );
        assert_eq!(rc, JNI_ERR);
        assert!(last_error_string().is_some());
    }

    #[test]
    fn last_error_clear_round_trip() {
        set_last_error("boom");
        assert_eq!(last_error_string().as_deref(), Some("boom"));
        cratonvm_clear_error(std::ptr::null_mut());
        assert!(last_error_string().is_none());
    }

    #[test]
    fn destroy_null_is_noop() {
        // Must not panic / segfault.
        cratonvm_destroy(std::ptr::null_mut());
    }

    #[test]
    fn release_ref_null_handle_returns_err() {
        clear_last_error();
        assert_eq!(cratonvm_release_ref(std::ptr::null_mut(), 0), JNI_ERR);
        assert!(last_error_string().is_some());
    }

    #[cfg(not(flat_api_live_vm))]
    #[test]
    fn flat_vm_state_allows_only_one_active_surface() {
        {
            let mut state = FLAT_VM_STATE.lock().expect("flat state lock");
            *state = None;
        }

        {
            let _creating = begin_flat_vm_create().expect("first flat create should claim");
            assert!(
                begin_flat_vm_create().is_err(),
                "overlapping flat create must be rejected"
            );
            assert!(
                flat_vm_blocks_invocation_create(),
                "Invocation API create must be blocked while flat create is in flight"
            );
        }
        assert!(
            !flat_vm_blocks_invocation_create(),
            "dropped in-flight flat create should release the process slot"
        );

        let mut active = begin_flat_vm_create().expect("flat create should claim again");
        active.activate(0xabc0).expect("activation should succeed");
        assert!(
            begin_flat_vm_create().is_err(),
            "active flat VM must block a second flat create"
        );
        assert!(
            flat_vm_blocks_invocation_create(),
            "active flat VM must block Invocation API create"
        );

        release_flat_vm(0xdef0);
        assert!(
            flat_vm_blocks_invocation_create(),
            "wrong VM key must not release the active slot"
        );
        release_flat_vm(0xabc0);
        assert!(
            !flat_vm_blocks_invocation_create(),
            "destroying the active flat VM should release the process slot"
        );
    }

    #[test]
    fn borrow_guard_rejects_reentrant_same_handle() {
        // A fabricated (never-dereferenced) handle address: the guard only keys
        // on the pointer value, never reads through it.
        let fake = 0xdead_beef_usize as *mut CratonVm;
        let g1 = BorrowGuard::acquire(fake).expect("first acquire must succeed");
        // A second acquire on the same handle (the re-entrant case) is rejected.
        assert!(
            BorrowGuard::acquire(fake).is_none(),
            "re-entrant acquire on the same handle must be rejected"
        );
        // A different handle is independent.
        let other = 0xfeed_face_usize as *mut CratonVm;
        let g2 = BorrowGuard::acquire(other).expect("distinct handle must acquire");
        drop(g2);
        // After the first guard drops, the handle is free again.
        drop(g1);
        let g3 = BorrowGuard::acquire(fake).expect("handle must be re-acquirable after release");
        drop(g3);
    }

    // Opt-in live-VM round trip: create → new_string → load_class →
    // invoke_static → destroy. Gated off the default run because it performs
    // the full VM bootstrap (and may require a host JDK on the classpath).
    // Run with: `cargo test -p libcratonvm --lib -- --ignored` is NOT enough
    // (it still compiles into the default binary); instead build with
    // `RUSTFLAGS="--cfg flat_api_live_vm"`.
    #[cfg(flat_api_live_vm)]
    #[test]
    fn flat_api_live_round_trip() {
        clear_last_error();
        let vm = cratonvm_create(std::ptr::null());
        assert!(!vm.is_null(), "create failed: {:?}", last_error_string());

        // load a bootstrap class.
        let name = CString::new("java/lang/System").unwrap();
        let mut cls: CratonClass = u64::MAX;
        assert_eq!(
            cratonvm_load_class(vm, name.as_ptr(), &mut cls),
            JNI_OK,
            "load_class failed: {:?}",
            last_error_string()
        );

        // create a string handle.
        let text = CString::new("embed").unwrap();
        let s = cratonvm_new_string(vm, text.as_ptr());
        assert_ne!(s, 0, "new_string failed: {:?}", last_error_string());

        // read the string handle back to UTF-8 and confirm the round trip.
        let back = cratonvm_string_utf8(vm, s);
        assert!(
            !back.is_null(),
            "string_utf8 failed: {:?}",
            last_error_string()
        );
        // SAFETY: `back` is a live caller-owned C string from cratonvm_string_utf8.
        let back_str = unsafe { CStr::from_ptr(back) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(back_str, "embed");
        cratonvm_free_string(back);

        // virtual dispatch: "embed".length() == 5 (descriptor excludes receiver).
        let len_m = CString::new("length").unwrap();
        let len_sig = CString::new("()I").unwrap();
        let len =
            cratonvm_invoke_virtual(vm, s, len_m.as_ptr(), len_sig.as_ptr(), std::ptr::null(), 0);
        assert_eq!(
            len.tag,
            craton_tag::INT,
            "length() failed: {:?}",
            last_error_string()
        );
        assert_eq!(len.payload as u32 as i32, 5);

        // object inspection: the string's runtime class is java/lang/String.
        let mut scls: CratonClass = u64::MAX;
        assert_eq!(
            cratonvm_object_class(vm, s, &mut scls),
            JNI_OK,
            "object_class failed: {:?}",
            last_error_string()
        );
        assert_ne!(scls, u64::MAX);
        let cname = cratonvm_class_name(vm, scls);
        assert!(
            !cname.is_null(),
            "class_name failed: {:?}",
            last_error_string()
        );
        // SAFETY: caller-owned C string from cratonvm_class_name.
        let cname_str = unsafe { CStr::from_ptr(cname) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(cname_str, "java/lang/String");
        cratonvm_free_string(cname);

        // field read-back: a String has a non-negative field count and slot 0
        // reads without error (the byte[]/coder layout is JDK-version-specific,
        // so we only assert the read-back path is sound, not a specific value).
        let fc = cratonvm_field_count(vm, s);
        assert!(fc >= 0, "field_count failed: {:?}", last_error_string());
        if fc > 0 {
            let f0 = cratonvm_get_field(vm, s, 0);
            assert_ne!(
                f0.tag,
                craton_tag::ERROR,
                "get_field(0) failed: {:?}",
                last_error_string()
            );
        }
        // out-of-range field index is a clean error, not a crash.
        let oob = cratonvm_get_field(vm, s, fc);
        assert_eq!(oob.tag, craton_tag::ERROR);

        // field-by-name resolution + read-back + write-back round trip on
        // String.hash (the non-final cached-hashCode int field present in the
        // JDK String layout).
        let hash_name = CString::new("hash").unwrap();
        let mut hash_idx: JInt = -1;
        assert_eq!(
            cratonvm_field_index(vm, scls, hash_name.as_ptr(), &mut hash_idx),
            JNI_OK,
            "field_index(String.hash) failed: {:?}",
            last_error_string()
        );
        assert!(hash_idx >= 0);
        // descriptor-aware resolution: the correct descriptor ("I") resolves to
        // the SAME slot as the name-only resolve; a wrong descriptor ("J") finds
        // no matching field; a null descriptor is name-only (same slot again).
        {
            let desc_i = CString::new("I").unwrap();
            let desc_j = CString::new("J").unwrap();
            let mut idx_i: JInt = -1;
            assert_eq!(
                cratonvm_field_index_desc(
                    vm,
                    scls,
                    hash_name.as_ptr(),
                    desc_i.as_ptr(),
                    &mut idx_i
                ),
                JNI_OK,
                "field_index_desc(String.hash, I) failed: {:?}",
                last_error_string()
            );
            assert_eq!(
                idx_i, hash_idx,
                "descriptor-matched slot must equal name-only slot"
            );
            let mut idx_j: JInt = -1;
            assert_eq!(
                cratonvm_field_index_desc(
                    vm,
                    scls,
                    hash_name.as_ptr(),
                    desc_j.as_ptr(),
                    &mut idx_j
                ),
                JNI_ERR,
                "field_index_desc(String.hash, J) should not match an int field"
            );
            let mut idx_n: JInt = -1;
            assert_eq!(
                cratonvm_field_index_desc(
                    vm,
                    scls,
                    hash_name.as_ptr(),
                    std::ptr::null(),
                    &mut idx_n
                ),
                JNI_OK
            );
            assert_eq!(
                idx_n, hash_idx,
                "null descriptor must be name-only resolution"
            );
        }
        // name-based read agrees with index-based read at the resolved slot.
        let by_name = cratonvm_get_field_by_name(vm, s, hash_name.as_ptr());
        let by_index = cratonvm_get_field(vm, s, hash_idx);
        assert_eq!(by_name.tag, craton_tag::INT);
        assert_eq!(by_name.payload, by_index.payload);
        // write-back by name, then read it back (round trip).
        assert_eq!(
            cratonvm_set_field_by_name(
                vm,
                s,
                hash_name.as_ptr(),
                CratonValue {
                    tag: craton_tag::INT,
                    payload: 0x4d2
                }
            ),
            JNI_OK,
            "set_field_by_name failed: {:?}",
            last_error_string()
        );
        let after = cratonvm_get_field_by_name(vm, s, hash_name.as_ptr());
        assert_eq!(after.payload as u32 as i32, 0x4d2);
        // an unknown field name is a clean error, not a crash.
        let bogus = CString::new("no_such_field_xyz").unwrap();
        let mut bogus_idx: JInt = -1;
        assert_eq!(
            cratonvm_field_index(vm, scls, bogus.as_ptr(), &mut bogus_idx),
            JNI_ERR
        );
        assert!(last_error_string().is_some());

        // invoke a void static (System.gc) with no args.
        let m = CString::new("gc").unwrap();
        let sig = CString::new("()V").unwrap();
        let r = cratonvm_invoke_static(
            vm,
            name.as_ptr(),
            m.as_ptr(),
            sig.as_ptr(),
            std::ptr::null(),
            0,
        );
        assert_ne!(
            r.tag,
            craton_tag::ERROR,
            "invoke failed: {:?}",
            last_error_string()
        );

        // a bogus class name sets the last error.
        let bad = CString::new("no/such/Class").unwrap();
        let mut bad_cls: CratonClass = 0;
        assert_eq!(cratonvm_load_class(vm, bad.as_ptr(), &mut bad_cls), JNI_ERR);
        assert!(last_error_string().is_some());

        assert_eq!(
            cratonvm_release_ref(vm, s),
            JNI_OK,
            "release_ref failed: {:?}",
            last_error_string()
        );

        cratonvm_destroy(vm);
    }

    // -- Foreign-thread attach concurrent-GC soak (opt-in) -----------------
    //
    // End-to-end validation of the foreign-thread-attach work
    // (`foreign-thread-attach.md` §6): `JNI_CreateJavaVM` once, then K host
    // (non-VM-created) OS threads each `AttachCurrentThread`, loop a static
    // method that allocates churny garbage while a moving young-gen GC fires
    // under a small heap, then `DetachCurrentThread`.
    //
    // Pass criteria: no UAF / SIGSEGV, no `wait_for_all` hang (a watchdog
    // aborts on deadlock), the process stays alive, every worker's calls
    // executed (proving real registration, not the env-only gate-off path),
    // and `alive_count` returns to the post-create baseline.
    //
    // Opt-in (JDK-dependent + slow + creates the process-global VM, so it must
    // not share a binary with the other live-VM test). Build with the cfg and
    // run with the gate on:
    //   $env:CRATONVM_FOREIGN_ATTACH=1
    //   $env:RUSTFLAGS="--cfg foreign_attach_soak"
    //   cargo test -p libcratonvm --release foreign_attach_concurrent_gc_soak -- --nocapture
    /// Debug helper: symbolize comma-separated `exe+0x<RVA>` addresses from a
    /// prior crash, against THIS (same) binary. Run immediately after a crash
    /// run (no rebuild) so the RVAs still map.
    #[cfg(foreign_attach_soak)]
    #[test]
    fn dbg_symbolize_rvas() {
        let spec = std::env::var("CRATONVM_DBG_RVAS").unwrap_or_default();
        let rvas: Vec<usize> = spec
            .split(',')
            .map(|s| s.trim().trim_start_matches("0x"))
            .filter(|s| !s.is_empty())
            .filter_map(|s| usize::from_str_radix(s, 16).ok())
            .collect();
        for (rva, name) in cratonvm_vm::runtime::crash_handler::symbolize_rvas(&rvas) {
            eprintln!(
                "0x{rva:x} => {}",
                name.unwrap_or_else(|| "<unresolved>".to_string())
            );
        }
    }

    #[cfg(foreign_attach_soak)]
    #[test]
    fn foreign_attach_concurrent_gc_soak() {
        use std::os::raw::c_char;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        // Foreign attach is default-ON (step 7); set it explicitly so the soak
        // is deterministic regardless of any ambient `CRATONVM_FOREIGN_ATTACH=0`
        // opt-out in the environment. Process-scoped: the attaching threads
        // this soak spawns below must see it too, and it is held for the whole
        // test body.
        let _foreign_attach = cratonvm_types::flags::override_process(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_FOREIGN_ATTACH",
                Some("1"),
            )]),
        );
        // Print a symbolized native backtrace on any access violation.
        cratonvm_vm::runtime::crash_handler::install_hardware_fault_handler();

        // Small heap → frequent young-gen GC under K-thread allocation churn.
        // Overridable via env for bisection.
        let xmx_s = std::env::var("CRATONVM_SOAK_XMX").unwrap_or_else(|_| "-Xmx48m".to_string());
        let xmx = CString::new(xmx_s).unwrap();
        let mut opts = [JavaVMOption {
            option_string: xmx.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        }];
        let mut args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 1,
        };

        let mut vm: JavaVM = std::ptr::null();
        let mut env: *mut c_void = std::ptr::null_mut();
        let rc = JNI_CreateJavaVM(
            &mut vm as *mut JavaVM,
            &mut env as *mut *mut c_void,
            &mut args as *mut JavaVMInitArgs as *mut c_void,
        );
        assert_eq!(
            rc,
            JNI_OK,
            "JNI_CreateJavaVM failed: {:?}",
            last_error_string()
        );
        assert!(!vm.is_null() && !env.is_null());

        // Read a JNIEnv/JavaVM function-table slot (the tables are
        // `*const *const usize`).
        let tbl_slot = |tbl: *const *const usize, n: usize| -> usize {
            // SAFETY: tbl points at a live, fully-populated function table.
            unsafe { *(*tbl).add(n) }
        };

        // Resolve `Integer.toString(int)` ONCE on the creating thread. Both
        // handles are GC-stable integers (a ClassId-as-handle and an encoded
        // method id), so they are shareable across threads.
        type FindClassFn = extern "C" fn(*mut c_void, *const c_char) -> u64;
        type GetStaticMidFn = extern "C" fn(*mut c_void, u64, *const c_char, *const c_char) -> u64;
        let find_class: FindClassFn =
            unsafe { std::mem::transmute(tbl_slot(env as *const *const usize, 6)) };
        let get_static_mid: GetStaticMidFn =
            unsafe { std::mem::transmute(tbl_slot(env as *const *const usize, 113)) };

        let cls_name = CString::new("java/lang/Integer").unwrap();
        let (m, sig) = match std::env::var("CRATONVM_SOAK_METHOD").as_deref() {
            // valueOf(small int) returns a cached box — exercises execution with
            // NO allocation, to separate alloc bugs from execution bugs.
            Ok("valueOf") => ("valueOf", "(I)Ljava/lang/Integer;"),
            _ => ("toString", "(I)Ljava/lang/String;"),
        };
        let m_name = CString::new(m).unwrap();
        let m_sig = CString::new(sig).unwrap();
        let cls = find_class(env, cls_name.as_ptr());
        assert_ne!(cls, 0, "FindClass(java/lang/Integer) failed");
        let mid = get_static_mid(env, cls, m_name.as_ptr(), m_sig.as_ptr());
        assert_ne!(mid, 0, "GetStaticMethodID(Integer.toString) failed");

        // System.gc()V — workers call it periodically to force STW collections
        // *while other foreign threads are mid-call*, which is precisely the
        // path foreign-thread GC participation must survive (a small heap alone
        // rarely fills fast enough under this modest churn).
        let sys_name = CString::new("java/lang/System").unwrap();
        let gc_name = CString::new("gc").unwrap();
        let gc_sig = CString::new("()V").unwrap();
        let sys_cls = find_class(env, sys_name.as_ptr());
        assert_ne!(sys_cls, 0, "FindClass(java/lang/System) failed");
        let gc_mid = get_static_mid(env, sys_cls, gc_name.as_ptr(), gc_sig.as_ptr());
        assert_ne!(gc_mid, 0, "GetStaticMethodID(System.gc) failed");

        // Sanity: the same call from the (properly attached) creating thread,
        // to separate harness/marshalling bugs from the foreign-attach path.
        {
            type CallObjAFn = extern "C" fn(*mut c_void, u64, u64, *const c_void) -> u64;
            let call: CallObjAFn =
                unsafe { std::mem::transmute(tbl_slot(env as *const *const usize, 116)) };
            let jv: i64 = 7;
            let s = call(env, cls, mid, &jv as *const i64 as *const c_void);
            // The creating thread has JNI_SHARED_VM but no JNI_THREAD, so the
            // env-table Call* path returns null for it (it drives the VM via the
            // flat API / vm.invoke instead). This is just diagnostic.
            eprintln!("[soak] main-thread Integer.toString(7) -> handle {s:#x}");
        }

        // Baseline alive thread count (just the creating thread, id 0) and GC
        // cycle counter (to prove a collection actually fired under the churn).
        let baseline = cratonvm_vm::native::jni::process_vm()
            .expect("process_vm published")
            .threads
            .thread_registry
            .alive_count();
        let gc_before = cratonvm_vm::native::jni::process_vm()
            .expect("process_vm published")
            .mem
            .heap
            .collection_count();

        // Share the process-stable handles into worker threads (raw pointers as
        // usize for Send; they address process-global singletons).
        #[derive(Clone, Copy)]
        struct Shared {
            vm: usize,
            cls: u64,
            mid: u64,
            sys_cls: u64,
            gc_mid: u64,
        }
        let shared = Shared {
            vm: vm as usize,
            cls,
            mid,
            sys_cls,
            gc_mid,
        };

        let parse_env = |k: &str, d: usize| -> usize {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let k_threads = parse_env("CRATONVM_SOAK_K", 6);
        let iters = parse_env("CRATONVM_SOAK_ITERS", 4000);
        let total_calls = StdArc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for t in 0..k_threads {
            let shared = shared;
            let total_calls = total_calls.clone();
            let iters = iters;
            // Host threads must give the VM interpreter enough native stack; the
            // debug interpreter recurses deeply (the VM's own worker carriers use
            // 8 MiB). Rust's ~2 MiB default thread stack is too small.
            handles.push(
                std::thread::Builder::new()
                    .name(format!("foreign-{t}"))
                    .stack_size(16 * 1024 * 1024)
                    .spawn(move || {
                        let vm = shared.vm as *const *const usize;
                        type AttachFn = extern "C" fn(
                            *const *const usize,
                            *mut *mut c_void,
                            *mut c_void,
                        ) -> JInt;
                        type DetachFn = extern "C" fn(*const *const usize) -> JInt;
                        type CallObjAFn =
                            extern "C" fn(*mut c_void, u64, u64, *const c_void) -> u64;
                        type DeleteLocalFn = extern "C" fn(*mut c_void, u64);
                        // SAFETY: vm is the process-global invocation table.
                        let attach: AttachFn = unsafe { std::mem::transmute(*(*vm).add(4)) };
                        let detach: DetachFn = unsafe { std::mem::transmute(*(*vm).add(5)) };

                        let mut wenv: *mut c_void = std::ptr::null_mut();
                        let arc = attach(vm, &mut wenv as *mut *mut c_void, std::ptr::null_mut());
                        assert_eq!(arc, JNI_OK, "AttachCurrentThread failed on worker {t}");
                        assert!(!wenv.is_null());

                        type CallVoidAFn = extern "C" fn(*mut c_void, u64, u64, *const c_void);
                        let call: CallObjAFn = unsafe {
                            std::mem::transmute(*(*(wenv as *const *const usize)).add(116))
                        };
                        let call_void: CallVoidAFn = unsafe {
                            std::mem::transmute(*(*(wenv as *const *const usize)).add(143))
                        };
                        let delete_local: DeleteLocalFn = unsafe {
                            std::mem::transmute(*(*(wenv as *const *const usize)).add(24))
                        };

                        let mut local = 0usize;
                        for i in 0..iters {
                            // jvalue: an `I` arg occupies the low 4 bytes of the union.
                            let jv: i64 = ((t * iters + i) as i32) as i64;
                            let s = call(
                                wenv,
                                shared.cls,
                                shared.mid,
                                &jv as *const i64 as *const c_void,
                            );
                            if s != 0 {
                                local += 1;
                                // Free the per-call result promptly to bound the live set.
                                delete_local(wenv, s);
                            }
                            // EVERY foreign thread periodically forces a stop-the-world
                            // GC while its siblings are mid-call — the strongest form of
                            // the participation path under test: N concurrent initiators
                            // racing `request_stw` (one wins, the losers fall through to
                            // `safepoint_check` and arrive). This previously deadlocked on
                            // the multi-thread-STW barrier bugs (generation-reuse in
                            // `arrive_and_wait`, the blocked-region wait-out, terminate-
                            // without-arrive, and a forced-GC young-arena over-expansion);
                            // those are fixed (see vm/src/threading/gc_barrier.rs and the
                            // `collect_garbage_inner` expansion gate), so the concurrent
                            // form is restored. NOTE: a *separate*, deeper residual —
                            // monitor-ownership desync in Java `Thread.join` under
                            // concurrent GC (scratch_churn/Churn.java) — does NOT affect
                            // this soak: foreign workers detach via the host join, never
                            // Java `Thread.join`.
                            if i % 64 == 0 {
                                call_void(wenv, shared.sys_cls, shared.gc_mid, std::ptr::null());
                            }
                        }
                        total_calls.fetch_add(local, Ordering::Relaxed);
                        let dr = detach(vm);
                        assert_eq!(dr, JNI_OK, "DetachCurrentThread failed on worker {t}");
                    })
                    .expect("failed to spawn foreign host thread"),
            );
        }

        // Deadlock watchdog: if the workers do not all finish within the
        // deadline, a `wait_for_all` hang (the failure this work prevents) is
        // the likely cause — abort loudly rather than hang the test runner.
        let finished = StdArc::new(AtomicBool::new(false));
        {
            let finished = finished.clone();
            let secs = std::env::var("CRATONVM_SOAK_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120u64);
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
                while std::time::Instant::now() < deadline {
                    if finished.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                eprintln!(
                    "foreign_attach_concurrent_gc_soak: watchdog timeout — likely STW deadlock"
                );
                if let Some(vm) = cratonvm_vm::native::jni::process_vm() {
                    eprintln!(
                        "[soak/watchdog] alive={} stw_requested={} blocked={} pending(expected-arrived)={}",
                        vm.threads.thread_registry.alive_count(),
                        vm.mem.gc_barrier
                            .stw_requested
                            .load(std::sync::atomic::Ordering::Acquire),
                        vm.mem.gc_barrier.blocked_count(),
                        vm.mem.gc_barrier.pending_count(),
                    );
                    for (tid, blocked, snap) in vm.threads.thread_registry.dump_blocked_states() {
                        eprintln!(
                            "[soak/watchdog]   tid={tid} blocked={blocked} snapshot_len={snap}"
                        );
                    }
                }
                std::process::abort();
            });
        }

        // The creating thread is now idle in host code while the foreign workers
        // drive Java + GC. It must declare itself in-native, or a worker's
        // stop-the-world would wait for it forever (it never reaches a Java
        // safepoint while parked in `join()`). This is exactly the host-facing
        // primitive the embedding API exposes for an idle coordinator thread.
        assert_eq!(cratonvm_thread_enter_native(), JNI_OK);

        for h in handles {
            h.join()
                .expect("a worker thread panicked (UAF/crash or failed assert)");
        }
        finished.store(true, Ordering::Release);

        // Rejoin the mutator population before inspecting VM state.
        assert_eq!(cratonvm_thread_leave_native(), JNI_OK);

        // Every worker's calls executed (proves real registration: the gate-off
        // env-only path would have returned 0 for every call).
        assert_eq!(
            total_calls.load(Ordering::Relaxed),
            k_threads * iters,
            "expected every static call to execute and allocate a String"
        );

        // alive_count returns to the post-create baseline: every attach was
        // matched by a detach that deregistered its thread.
        let after = cratonvm_vm::native::jni::process_vm()
            .expect("process_vm still live")
            .threads
            .thread_registry
            .alive_count();
        assert_eq!(
            after, baseline,
            "alive_count must return to baseline after all detaches"
        );

        // A moving collection must actually have fired under the churn,
        // otherwise the soak proves nothing about GC-safety. (Skipped only when
        // the caller forced a large heap / few iterations for bisection.)
        let gc_after = cratonvm_vm::native::jni::process_vm()
            .expect("process_vm still live")
            .mem
            .heap
            .collection_count();
        if iters >= 64 {
            assert!(
                gc_after > gc_before,
                "expected at least one GC cycle (periodic System.gc + churn) \
                 (before={gc_before}, after={gc_after})"
            );
        }
        eprintln!(
            "[soak] OK: {} calls across {k_threads} foreign threads, {} GC cycles",
            total_calls.load(Ordering::Relaxed),
            gc_after - gc_before
        );
    }
}
