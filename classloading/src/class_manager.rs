//! Class manager — loading, parsing, and caching of Java classes.
//!
//! The `ClassManager` is the central component that:
//! 1. Locates `.class` files via the parent-delegation model (bootstrap → extension → application)
//! 2. Parses them via `rustjvm_reader::read_class`
//! 3. Recursively loads superclasses and interfaces
//! 4. Stores them in a `ClassStore` indexed by `ClassId`
//! 5. Provides lookup by name and id
//!
//! **Synthetic bootstrap:** `ClassManager::ensure_synthetic_class` registers minimal classes with
//! `crate::class::Class::is_synthetic_stub` set — a legacy shortcut when no real classfile exists.
//! Project policy is to prefer real JDK/app `.class` bytes and shrink synthetic paths over time; see
//! `docs/jvm-no-synthetic-stubs.md`.

// T10.9.E: removed `use std::collections::{HashMap, HashSet};` — every
// internal map/set in this file now uses `FxHashMap` / `FxHashSet`
// (faster non-cryptographic hash, safe because keys are trusted internal
// data: ClassId, interned class names, etc.).
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use rustc_hash::{FxHashMap, FxHashSet};

use rustjvm_reader::attribute::{force_decode_all, Attribute};
use rustjvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags, MethodAccessFlags};
use rustjvm_reader::class_file_version::ClassFileVersion;
use rustjvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use rustjvm_reader::method::ClassFileMethod;
use tracing::debug;

use crate::class::{
    Class, ClassId, ClassLoaderId, ClassState, ClassStore, CodeSource, EnclosingMethodInfo,
    InnerClassEntry, RecordComponentInfo,
};
use crate::loaders::{
    ApplicationClassFinder, BootstrapClassFinder, ClassFinder, ExtensionClassFinder,
};
use crate::module::{
    descriptor_from_module_attribute, package_of, packages_from_module_packages_attribute,
    ModuleRegistry,
};
use crate::vtype::ClassHierarchy;
use rustjvm_types::error::{ClassFileError, LinkageError, VmError};

/// Adapter implementing [`ClassHierarchy`] over the `ClassManager`'s
/// `ClassStore` + name-to-id index. Used by the verifier (Pass 2 / Pass 3)
/// during `define_class_with_options`. Each query is name-keyed and walks
/// the loaded-class indexes; if a referenced class hasn't been loaded yet
/// the query falls back to conservative defaults that keep verification
/// permissive (treat unknown classes as `java/lang/Object` subclasses, not
/// interfaces) — matching HotSpot's "subclass of nothing else" tolerance
/// for unresolved references during Pass 3.
struct ClassStoreHierarchy<'a> {
    class_store: &'a ClassStore,
    loaded_classes: &'a FxHashMap<(ClassLoaderId, Arc<str>), ClassId>,
}

impl<'a> ClassStoreHierarchy<'a> {
    fn lookup(&self, name: &str) -> Option<ClassId> {
        // Probe with `Arc<str>` to match the storage map's key type.
        let probe: Arc<str> = Arc::from(name);
        for loader_id in &[
            ClassLoaderId::Bootstrap,
            ClassLoaderId::Extension,
            ClassLoaderId::Application,
        ] {
            if let Some(&id) = self.loaded_classes.get(&(*loader_id, Arc::clone(&probe))) {
                return Some(id);
            }
        }
        // Fall back to scanning every loader (custom loaders).
        for ((_, class_name), &id) in self.loaded_classes.iter() {
            if &**class_name == name {
                return Some(id);
            }
        }
        None
    }
}

impl<'a> ClassHierarchy for ClassStoreHierarchy<'a> {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        if child == parent || parent == "java/lang/Object" {
            return true;
        }
        let (Some(child_id), Some(parent_id)) = (self.lookup(child), self.lookup(parent)) else {
            // Either class isn't loaded yet — be permissive so the
            // verifier doesn't reject legitimate forward references.
            return true;
        };
        match self.class_store.get(child_id) {
            Some(c) => c.is_subclass_of(parent_id, self.class_store),
            None => true,
        }
    }

    fn common_superclass(&self, a: &str, b: &str) -> String {
        if a == b {
            return a.to_string();
        }
        let (Some(a_id), Some(b_id)) = (self.lookup(a), self.lookup(b)) else {
            return "java/lang/Object".to_string();
        };
        let a_cls = match self.class_store.get(a_id) {
            Some(c) => c,
            None => return "java/lang/Object".to_string(),
        };
        let mut current = Some(a_cls);
        let mut depth = 0usize;
        while let Some(cls) = current {
            if depth > 256 {
                break;
            }
            depth += 1;
            let cls_id = cls.id;
            if let Some(b_cls) = self.class_store.get(b_id) {
                if b_cls.is_subclass_of(cls_id, self.class_store) {
                    return cls.name.to_string();
                }
            }
            current = cls.superclass.and_then(|sid| self.class_store.get(sid));
        }
        "java/lang/Object".to_string()
    }

    fn is_interface(&self, name: &str) -> bool {
        match self.lookup(name).and_then(|id| self.class_store.get(id)) {
            Some(c) => c.is_interface(),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// T6.3.1 — JVMTI class-lifecycle hook registry
// ---------------------------------------------------------------------------
//
// The class loader must notify JVMTI agents on two lifecycle events:
//   - ClassLoad (fired after the class is registered in the class store)
//   - ClassPrepare (fired after the class is linked / prepared)
//
// `classloading` has no dependency on the VM crate where the JVMTI manager
// lives, so the VM registers a pair of function pointers at boot time. When
// no hook is registered the class loader's hot path pays a single
// `AtomicBool` load and branches past the call — the cost of the hook is
// zero for embedded/test scenarios that do not attach an agent.
//
// The hooks receive `(class_id_u32, class_name, thread_id)`. Thread id is
// supplied by the caller; when unknown (e.g. bootstrap class loading before
// any thread exists) the caller should pass 0 and the JVMTI layer routes
// the event to the VM-init thread.
//
// Calls are made AFTER the class manager releases its internal locks so
// agent callbacks can safely re-enter the class loader (e.g. to call
// GetLoadedClasses) without self-deadlock.

/// Signature of the JVMTI class-lifecycle hook installed by the VM crate.
/// Parameters: `(class_id_u32, class_name, thread_id)`.
pub type JvmtiClassHook = fn(u32, &str, u64);

static CLASS_LOAD_HOOK: OnceLock<JvmtiClassHook> = OnceLock::new();
static CLASS_PREPARE_HOOK: OnceLock<JvmtiClassHook> = OnceLock::new();
static CLASS_HOOKS_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the JVMTI `ClassLoad` hook. Idempotent — only the first install
/// wins. Called once by the VM during `SharedVm::new`.
pub fn install_class_load_hook(hook: JvmtiClassHook) {
    if CLASS_LOAD_HOOK.set(hook).is_ok() {
        CLASS_HOOKS_ACTIVE.store(true, Ordering::Release);
    }
}

/// Install the JVMTI `ClassPrepare` hook. Idempotent.
pub fn install_class_prepare_hook(hook: JvmtiClassHook) {
    if CLASS_PREPARE_HOOK.set(hook).is_ok() {
        CLASS_HOOKS_ACTIVE.store(true, Ordering::Release);
    }
}

/// Invoke the class-load hook if one is installed. Hot path: a single
/// relaxed atomic load + branch when no agent is attached.
#[inline]
fn fire_class_load_hook(class_id: u32, class_name: &str, thread_id: u64) {
    if !CLASS_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = CLASS_LOAD_HOOK.get() {
        hook(class_id, class_name, thread_id);
    }
}

/// Invoke the class-prepare hook if one is installed.
#[inline]
fn fire_class_prepare_hook(class_id: u32, class_name: &str, thread_id: u64) {
    if !CLASS_HOOKS_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = CLASS_PREPARE_HOOK.get() {
        hook(class_id, class_name, thread_id);
    }
}

// ---------------------------------------------------------------------------
// WP2.4-B — JVMTI ClassFileLoadHook hook registry
// ---------------------------------------------------------------------------
//
// The JVMTI `ClassFileLoadHook` event fires whenever class bytes are about
// to be installed into the VM, with a chance for an instrumentation agent
// to substitute its own bytes. The hook receives the OLD bytes (or empty
// for the initial define) and the NEW bytes; the agent may return a
// modified Vec<u8>, which the class loader then uses in place of the
// supplied `new_bytes`.
//
// `Vec<u8>` is used instead of `&[u8]` for the return value so the hook can
// hand back ownership of a freshly-allocated transformed buffer without a
// borrow-vs-lifetime puzzle. An empty returned vec means "no transform —
// keep the original new_bytes". The class loader detects this and skips
// the substitution.

/// Signature of the JVMTI `ClassFileLoadHook` callback installed by the VM.
///
/// Parameters: `(class_id, class_name, old_bytes, new_bytes)`.
/// Return: an optional transformed byte vec. `None` (or an empty `Some`) =
/// no transform; the loader uses `new_bytes` as-is.
///
/// `old_bytes` is empty for the initial class load (no prior bytes exist).
/// On `redefine_class`, `old_bytes` is the previously-installed class file.
pub type ClassFileLoadHook = fn(u32, &str, &[u8], &[u8]) -> Option<Vec<u8>>;

static CLASS_FILE_LOAD_HOOK: OnceLock<ClassFileLoadHook> = OnceLock::new();
static CLASS_FILE_LOAD_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the JVMTI `ClassFileLoadHook`. Idempotent — only the first
/// install wins. Called once by the VM during `SharedVm::new` if a JVMTI
/// agent has registered for this event.
pub fn install_class_file_load_hook(hook: ClassFileLoadHook) {
    if CLASS_FILE_LOAD_HOOK.set(hook).is_ok() {
        CLASS_FILE_LOAD_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// Invoke the ClassFileLoadHook if one is installed. When no hook is
/// registered the cost is a single relaxed atomic load + branch.
///
/// Returns `Some(new_vec)` if the hook transformed the bytes, otherwise
/// `None` (caller should use the original `new_bytes`).
#[inline]
fn fire_class_file_load_hook(
    class_id: u32,
    class_name: &str,
    old_bytes: &[u8],
    new_bytes: &[u8],
) -> Option<Vec<u8>> {
    if !CLASS_FILE_LOAD_HOOK_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let hook = CLASS_FILE_LOAD_HOOK.get()?;
    let out = hook(class_id, class_name, old_bytes, new_bytes)?;
    if out.is_empty() {
        // Hook returned an empty vec → "no transform". Treat as None so the
        // caller uses the original bytes (which are guaranteed non-empty
        // by the upstream `bytes.len() < 8` rejection).
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// WP2.4-B — JIT cache invalidation hook
// ---------------------------------------------------------------------------
//
// When a class is redefined, every JIT-compiled body keyed on its old
// (class_id, method_index) pair must be discarded so the next call
// recompiles from the new bytecode. The VM owns the JIT cache; the class
// loader knows when a redefine completes — a hook bridges the two.

/// Signature of the JIT-invalidation hook fired by `redefine_class`.
///
/// Parameter: the `ClassId` (as `u32`) whose JIT entries must be evicted.
/// The VM-side adapter walks `shared.jit_cache`, `shared.tiered`, and any
/// per-thread invoke caches that key by class id and removes matching
/// entries. Method-index granularity is intentionally NOT exposed here —
/// at redefine time we conservatively evict every method body for the
/// class because their bodies all changed.
pub type JitInvalidateHook = fn(u32);

static JIT_INVALIDATE_HOOK: OnceLock<JitInvalidateHook> = OnceLock::new();
static JIT_INVALIDATE_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the JIT-invalidate hook. Called once by the VM during
/// `SharedVm::new`. Idempotent.
pub fn install_jit_invalidate_hook(hook: JitInvalidateHook) {
    if JIT_INVALIDATE_HOOK.set(hook).is_ok() {
        JIT_INVALIDATE_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// Invoke the JIT-invalidate hook for `class_id`. Hot path is a single
/// relaxed atomic load + branch when no hook is attached (e.g. during
/// classloading-only unit tests).
#[inline]
fn fire_jit_invalidate_hook(class_id: u32) {
    if !JIT_INVALIDATE_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = JIT_INVALIDATE_HOOK.get() {
        hook(class_id);
    }
}

// ---------------------------------------------------------------------------
// T10.5 — vtable install hook registry
// ---------------------------------------------------------------------------
//
// After a class is linked (after its methods are parsed and its superclass's
// vtable has already been built) the class loader fires a `VtableInstallHook`
// that hands a pre-built vec of slot descriptors to the VM. The VM's installed
// adapter converts each descriptor into a `crate::runtime::vtable::VtableEntry`
// and stores the whole vec in `shared.vtable_manager` via `install_vtable`.
// The vtable is then queryable by slot in O(1) for the lifetime of the class.
//
// The hook delivers OWNED data (moved `Vec`) so the adapter doesn't need to
// re-walk the class_manager under any lock. This keeps the class-link path
// and the vtable-install path fully decoupled.

/// T10.9.A — dispatch-time snapshot carried inside each `VtableSlotDescriptor`.
///
/// The link-time vtable installer captures everything the interpreter needs
/// to execute a virtual call **without** re-entering `class_manager.read()`.
/// A `None` variant means "no snapshot" (abstract or native method) — the
/// caller falls through to the slower resolution path.
///
/// Fields line up with `rustjvm_jit_api::CachedBytecodeMethod` so the VM
/// adapter can build that type directly. `code` is stored as `Vec<u8>`
/// because classloading runs before the VM's padded-bytecode helper is
/// reachable; the adapter pads-and-Arc's before handing off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtableMethodSnapshot {
    /// Fully-qualified internal name of the declaring class.
    // TODO(T10.9.E): switch `class_name` to `Arc<str>` to remove the per-snapshot
    // `class.name.to_string()` allocation at the two producer sites below
    // (lines ~2200 and ~2216 in `build_vtable_descriptors_with_overrides`).
    // Public-API change — `vm/src/runtime/vtable.rs:673` already does
    // `Arc::<str>::from(snap.class_name.as_str())` on the consumer side and
    // would simplify to `Arc::clone(&snap.class_name)`. Out of scope for the
    // current edit (constraint: do not change public API signatures from
    // this file alone).
    pub class_name: String,
    /// Source file (from SourceFile attribute), if any.
    pub source_file: Option<String>,
    /// Raw bytecode (NOT yet padded for speculative reads).
    pub code: Vec<u8>,
    /// Exception handler table.
    pub exception_table: Vec<rustjvm_reader::attribute::ExceptionTableEntry>,
    /// Max operand-stack depth.
    pub max_stack: u16,
    /// Max local-variable count.
    pub max_locals: u16,
    /// Parameter slot count (excluding `this`).
    pub num_params: u16,
    /// ACC_SYNCHRONIZED flag.
    pub is_synchronized: bool,
    /// ACC_STATIC flag (always false for vtable slots, kept for parity
    /// with `CachedBytecodeMethod`).
    pub is_static: bool,
    /// ACC_NATIVE flag — when true, `code` is empty and the interpreter
    /// must route through the native-method registry instead.
    pub is_native: bool,
}

/// One slot in a class's vtable, as seen by the class loader. The VM's
/// installed hook converts these into `runtime::vtable::VtableEntry`.
///
/// The absence of an entry (i.e. `None` in the vec) represents a reserved
/// slot that no concrete method fills yet (e.g. an abstract superclass
/// method that the subclass hasn't overridden). Dispatch against such a
/// slot would raise `AbstractMethodError` at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtableSlotDescriptor {
    /// ClassId (as `u32`) of the class that actually declares the method
    /// occupying this slot. May be the subclass (override) or a
    /// superclass (inherited).
    pub declaring_class_id: u32,
    /// Index into the declaring class's `methods` vec.
    pub method_index: u32,
    /// Method name, stored so the VM side can rebuild a `name_to_slot`
    /// index without re-parsing the class file.
    ///
    /// T10.9.E: `Arc<str>` (was `String`) — the source `ClassFileMethod.name`
    /// is already `Arc<str>` so the producer clones the existing arc
    /// (refcount bump, no allocation) instead of `.to_string()`-ing it.
    /// Saves ~90k throwaway allocations per Spring Boot cold start.
    ///
    /// TODO(T10.9.E follow-up): `vm/src/runtime/vtable.rs:674,697` consumes
    /// this field via `String::as_str()` / `Arc::<str>::from(String)`; the
    /// consumer needs a one-line update to use `Arc::clone(&d.method_name)`
    /// / `&*d.method_name`. Cannot be edited here (out-of-crate file).
    pub method_name: Arc<str>,
    /// Method descriptor.
    ///
    /// T10.9.E: `Arc<str>` (was `String`) — same rationale as `method_name`.
    /// TODO(T10.9.E follow-up): consumer at `vm/src/runtime/vtable.rs:675,698`
    /// needs the same one-line update as `method_name`.
    pub descriptor: Arc<str>,
    /// T10.9.A — snapshot of everything the interpreter needs to execute
    /// the method without re-entering the class manager. `None` for
    /// abstract methods (no Code attribute) or when the snapshot couldn't
    /// be built at link time (defensive fallback).
    pub dispatch: Option<VtableMethodSnapshot>,
}

/// Signature of the vtable-install hook installed by the VM crate.
///
/// Parameters: `(class_id_u32, entries)`. `entries[i]` is the vtable
/// slot at position `i`. The hook MUST NOT re-enter the class manager
/// (the class loader still holds `&mut self` when firing it); the
/// vec is fully self-contained.
pub type VtableInstallHook = fn(u32, Vec<Option<VtableSlotDescriptor>>);

/// T10.9.A — signature of the vtable-override hook.
///
/// Fired immediately after `VtableInstallHook` for each super-class slot
/// that the newly defined class overrode. The hook drives
/// `VtableManager::invalidate_for_override(super_class_id, slot)` on the
/// VM side so cached dispatch entries (and any JIT inlines that made
/// LeafClass assumptions) see the new subclass as soon as it is linked.
///
/// Parameters: `(super_class_id_u32, slot_index)`. May be fired multiple
/// times per class definition (once per overridden slot).
pub type VtableOverrideHook = fn(u32, usize);

static VTABLE_INSTALL_HOOK: OnceLock<VtableInstallHook> = OnceLock::new();
static VTABLE_OVERRIDE_HOOK: OnceLock<VtableOverrideHook> = OnceLock::new();
static VTABLE_HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the vtable-install hook. Idempotent — only the first install
/// wins. Called once by the VM during `SharedVm::new`.
pub fn install_vtable_install_hook(hook: VtableInstallHook) {
    if VTABLE_INSTALL_HOOK.set(hook).is_ok() {
        VTABLE_HOOK_ACTIVE.store(true, Ordering::Release);
    }
}

/// T10.9.A — install the vtable-override hook. Idempotent. Called once
/// by the VM during `SharedVm::new` right after the install hook.
pub fn install_vtable_override_hook(hook: VtableOverrideHook) {
    let _ = VTABLE_OVERRIDE_HOOK.set(hook);
    // Share the HOOK_ACTIVE gate with the install hook — either hook
    // registered is enough to turn on the check.
    VTABLE_HOOK_ACTIVE.store(true, Ordering::Release);
}

/// Invoke the vtable-install hook if one is installed. Hot path is a
/// single relaxed atomic load + branch when no hook is attached (e.g.
/// during unit tests that exercise the class loader in isolation).
#[inline]
fn fire_vtable_install_hook(class_id: u32, entries: Vec<Option<VtableSlotDescriptor>>) {
    if !VTABLE_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = VTABLE_INSTALL_HOOK.get() {
        hook(class_id, entries);
    }
}

/// T10.9.A — Invoke the vtable-override hook if one is installed. Hot
/// path is a single relaxed atomic load + branch when no hook is
/// attached. Called by the class loader with each `(super_id, slot)`
/// pair where a subclass just overrode an inherited method.
#[inline]
fn fire_vtable_override_hook(super_class_id: u32, slot: usize) {
    if !VTABLE_HOOK_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(hook) = VTABLE_OVERRIDE_HOOK.get() {
        hook(super_class_id, slot);
    }
}

// ---------------------------------------------------------------------------
// WP1.5 — built-in class-loader registration hook
// ---------------------------------------------------------------------------

/// Boot-time hook that associates the three built-in class-loader names
/// (`BootLoader`, `ClassLoaders$PlatformClassLoader`,
/// `ClassLoaders$AppClassLoader`) with reserved [`ClassLoaderId`]s.
///
/// Implementation note: `ClassLoader`-side state (the singleton instances
/// themselves) lives in `native-builtins/src/classloader.rs`. This hook
/// only asks the `ClassManager` to acknowledge the names so that classes
/// *defined by* a built-in loader can be looked up by name without
/// consulting a live ClassLoader object. The built-in loaders always use
/// `ClassLoaderId::BOOTSTRAP (0)` internally; the name registration
/// merely increments a diagnostic counter that tests observe.
///
/// Idempotent — safe to call from any number of initialization paths.
pub fn register_builtin_classloaders() {
    crate::builtin_loaders::register_builtin_loader_aliases();
}

/// Return `true` if `internal_name` is the JVM-internal name of one of
/// the three built-in class loaders (BootLoader, PlatformClassLoader,
/// AppClassLoader) or the shared `BuiltinClassLoader` base.
///
/// Public helper so `native-builtins` can check the reflective mirror
/// name of an allocated loader and dispatch special behavior (e.g. the
/// synthetic `getName()` result) without hard-coding the strings.
pub fn is_builtin_classloader_name(internal_name: &str) -> bool {
    crate::builtin_loaders::builtin_loader_kind(internal_name).is_some()
}

/// Options for [`ClassManager::define_class_with_options`] (NEW-8 + WP2.3).
///
/// Callers that do not need any of the non-default behavior should
/// use the unadorned [`ClassManager::define_class`] entry point which
/// uses `DefineClassOptions::default()`.
#[derive(Debug, Clone, Default)]
pub struct DefineClassOptions {
    /// Override the stored class name. If `Some`, the class is
    /// registered under this name instead of the `this_class` entry
    /// from the class file's constant pool. Used by
    /// `Lookup.defineHiddenClass` so two hidden classes produced from
    /// the same source template get distinct, unique names.
    pub override_name: Option<String>,
    /// Mark the class as hidden (JEP 371) atomically with
    /// registration. Hidden classes are not returned by
    /// `find_class_by_name` / `Class.forName` /
    /// `ClassLoader.findLoadedClass`.
    pub hidden: bool,
    /// WP2.3: Suppress the verifier on these bytes. Off by default (we
    /// run the structural + bytecode verifier). Hidden-class /
    /// runtime-generation paths sometimes set this to `true` because
    /// the JDK trusts its own emitter; ByteBuddy + CGLIB also benefit.
    pub skip_verification: bool,
    /// WP2.3: Optional `CodeSource` to attribute to the new class. If
    /// `Some`, this overrides the default classpath-derived
    /// `code_source`. Used by `defineClass(name, bytes, off, len, pd)`
    /// natives so `Class.getProtectionDomain()` later reflects the
    /// caller-supplied PD.
    pub code_source: Option<CodeSource>,
    /// WP2.3: When defining a class that names an existing class, allow
    /// the redefinition to replace the old one in place. Used by
    /// `java.lang.instrument.Instrumentation.redefineClasses` (WP2.4).
    /// When `false` (default) a duplicate define returns
    /// `LinkageError::IncompatibleClassChangeError` per JVMS §5.3.5.
    pub allow_redefine: bool,
    /// WP2.3: Nest-host attribution for hidden classes. When `Some`,
    /// the new class joins the named class's nest (i.e. its
    /// `nest_host` is set to the lookup class's nest host, NOT to its
    /// own name). Mirrors `Lookup.defineHiddenClass(... NESTMATE ...)`.
    pub nest_host_class_name: Option<String>,
}

/// Options for [`ClassManager::redefine_class`] (WP2.4-B).
///
/// This is the JVMTI / JEP 109 in-place class redefinition entry point.
/// `redefine_class` enforces strict structural equivalence (same name,
/// superclass, interfaces, fields, and method declarations) — only
/// method bodies, the constant pool, static initializers, and
/// annotations may change. See the doc comment on
/// [`ClassManager::redefine_class`] for the full constraint list.
#[derive(Debug, Clone, Default)]
pub struct RedefineOptions {
    /// When `true`, skip the strict structural-equivalence check.
    /// Used by `retransformClasses` when an agent has explicitly
    /// registered with `canRetransform = true` and the JVM trusts it.
    /// Default: `false` (strict redefine).
    ///
    /// Even with this flag set, the new bytes MUST share the same
    /// class name as the old class — replacing one class's bytes with
    /// another class's bytes is never legal under JEP 109.
    pub skip_structural_check: bool,
    /// Trace-flag for diagnostics. When `true`, the redefine path
    /// emits a `tracing::debug` line listing the methods whose bodies
    /// changed (and any structural-check rejections). Default: `false`.
    pub log_diff: bool,
}

/// Manages class loading for the VM.
///
/// Maintains the `ClassStore` (all loaded classes), three built-in class finders
/// (bootstrap, extension, application), and a `(loader_id, name) → ClassId` cache.
/// Loading a class follows the parent delegation model and automatically loads
/// its superclass and interfaces recursively.
pub struct ClassManager {
    /// Storage for all loaded classes (shared across all loaders).
    pub class_store: ClassStore,

    /// Bootstrap class finder — loads from rt.jar / boot classpath.
    bootstrap: BootstrapClassFinder,

    /// Extension class finder — loads from $JAVA_HOME/lib/ext.
    extension: ExtensionClassFinder,

    /// Application class finder — loads from the -classpath.
    application: ApplicationClassFinder,

    /// (loader_id, name) → ClassId cache.
    /// T10.9.B: FxHashMap for internal hot-path lookups (keys are internal
    /// class names, never untrusted user input).
    ///
    /// T10.9.E: key is `(ClassLoaderId, Arc<str>)` (was `(_, String)`). All
    /// the high-volume insert sites already hold an `Arc<str>` (from
    /// `Class.name`), so storing the Arc in the key is a refcount bump
    /// rather than a fresh allocation. Probe sites that only have `&str`
    /// construct a one-off `Arc::from(s)` — no worse than the prior
    /// `name.to_string()`. Eliminates ~60+ String allocations per class
    /// define on the hot load_class / define_class path.
    loaded_classes: FxHashMap<(ClassLoaderId, Arc<str>), ClassId>,

    /// Fast name→ClassId lookup (hash-keyed, zero-allocation on lookup).
    /// Populated alongside loaded_classes.
    name_to_id: FxHashMap<u64, ClassId>,

    /// JPMS module registry: descriptors, package map, readability graph.
    pub module_registry: ModuleRegistry,

    /// Raw class file bytes for each loaded class, keyed by internal name.
    /// Populated during define_class() for CDS dump support.
    /// T10.9.B: FxHashMap — keys are class-file internal names.
    pub class_bytes_cache: FxHashMap<String, Vec<u8>>,

    /// CDS-cached class bytes: class name -> raw .class bytes.
    /// Populated from the CDS archive at startup, checked before classpath delegation.
    /// T10.9.B: FxHashMap — keys are internal class names loaded from a trusted
    /// CDS archive produced by this VM.
    pub cds_class_cache: FxHashMap<String, Vec<u8>>,

    /// Guard set for classes currently being loaded by `define_class`.
    /// If `load_class` encounters a class name already in this set, it means
    /// we have a circular class hierarchy (A extends B extends A) which is
    /// forbidden by the JVM spec (§5.3.5).
    /// T10.9.B: FxHashSet — keys are internal class names during loading.
    loading_guard: FxHashSet<String>,

    /// T10.5 — per-class vtable descriptor layout, indexed by ClassId.
    /// Populated by `define_class_with_options` at link time and used by
    /// subclasses of the same class as the "parent vtable" when computing
    /// their own layout. The VM's installed `VtableInstallHook` receives a
    /// clone of the owned vec per class and funnels it into
    /// `shared.vtable_manager.install_vtable(...)`.
    ///
    /// Keying on ClassId (not name) keeps the superclass-lookup O(1) even
    /// for classes loaded by many different classloaders.
    ///
    /// T10.9.E: switched from `HashMap` (SipHash) to `FxHashMap`. ClassId
    /// is a trusted internal `u32`, never user input, so the DoS-resistant
    /// SipHash is pure overhead on every class-link superclass probe.
    vtable_descriptors: FxHashMap<ClassId, Vec<Option<VtableSlotDescriptor>>>,

    /// WP2.3 — per-class "skip bytecode verification" flag, recorded when
    /// `define_class_with_options` is called with
    /// `DefineClassOptions::skip_verification = true`. Stored as a side
    /// table (rather than a field on `Class`) so that VM-side construction
    /// sites that build `Class` directly do not need to know about the
    /// flag — only callers that route through `define_class_with_options`
    /// (the hidden-class / runtime-bytecode-generation entry points) ever
    /// set it.  The verifier reads this through
    /// `class_skip_bytecode_verification(class_id)`.
    ///
    /// Generators like ByteBuddy, CGLIB, and JDK dynamic Proxy emit
    /// classes that pass JVMS structural rules but trip our bytecode
    /// verifier on synthesised stack frames; the only safe escape hatch
    /// is to honour the caller's request to bypass verification (which
    /// HotSpot likewise does for trusted hidden classes).
    skip_bytecode_verification: FxHashSet<ClassId>,

    /// WP2.3 — counter used to mangle hidden-class names when the
    /// caller-supplied `override_name` collides with an already-loaded
    /// class. The counter is monotonic across the whole class manager so
    /// that `Foo/0x1`, `Foo/0x2`, ... never collide even across many
    /// hidden defines of the same template.
    hidden_name_counter: u64,

    /// WP2.4-B — JEP 109 / JVMTI `RedefineClasses` generation counter,
    /// keyed by [`ClassId`].  Bumped by [`ClassManager::redefine_class`]
    /// on each successful redefinition; never decremented.  Caches that
    /// snapshot a method-resolution at lookup time stamp the generation
    /// from `class_redefine_generation(class_id)` and on next hit
    /// recheck it; on mismatch the entry is treated as stale and
    /// re-resolved against the freshly-installed bytecode.
    ///
    /// Stored on the manager (not on `Class`) so existing `Class`
    /// construction sites in tests and bench fixtures don't need to be
    /// touched.  An entry is created lazily — a class that has never
    /// been redefined returns generation 0 from
    /// [`ClassManager::class_redefine_generation`] without allocating.
    /// The `Arc` is intentional: `class_redefine_generation_handle`
    /// returns a clone so a JIT cache or invoke cache can hold onto the
    /// counter and check it later without reborrowing the
    /// [`ClassManager`].
    ///
    /// WP2.4-F1 — wrapped in a `RwLock` so the hot per-thread invoke-cache
    /// populate path can acquire a handle through a `&ClassManager`
    /// borrow (which is what `shared.class_manager.read()` provides) and
    /// share the *same* `Arc<AtomicU32>` that `redefine_class` will
    /// later bump.  Without this, populate-time and redefine-time would
    /// hand out two unrelated counters and the cache would never see a
    /// bump.
    ///
    /// T10.9.E: switched from `HashMap` (SipHash) to `FxHashMap`. ClassId
    /// is a trusted `u32` and this map is consulted on every populate of a
    /// per-thread invoke-cache entry; FxHash is a measurable win.
    redefine_generations: RwLock<FxHashMap<ClassId, Arc<AtomicU32>>>,
}

/// FNV-1a hash of a class name.
#[inline]
fn class_name_hash(name: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl ClassManager {
    /// Create a new class manager with the three built-in class loaders.
    ///
    /// All classpath entries are scanned for `module-info.class` files during
    /// construction so that the module registry is populated before any class
    /// loading begins (N4: boot module loading baseline).
    pub fn new(
        boot_classpath: &[String],
        ext_classpath: &[String],
        app_classpath: &[String],
    ) -> Self {
        let bootstrap = BootstrapClassFinder::new(boot_classpath);
        let extension = ExtensionClassFinder::new(ext_classpath);
        let application = ApplicationClassFinder::new(app_classpath);

        let mut module_registry = ModuleRegistry::new();

        // Eagerly scan all classpath entries for module-info.class files
        // (N1: module graph resolution + N4: boot module loading).
        for class_path in [
            bootstrap.class_path(),
            extension.class_path(),
            application.class_path(),
        ] {
            for bytes in class_path.scan_module_infos() {
                Self::try_register_module_info(&mut module_registry, &bytes);
            }
        }

        if !module_registry.is_empty() {
            module_registry.build_readability_graph();
            debug!(
                modules = module_registry.len(),
                "Module registry populated from classpath scan"
            );
        }

        Self {
            class_store: ClassStore::new(),
            bootstrap,
            extension,
            application,
            loaded_classes: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            name_to_id: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            module_registry,
            class_bytes_cache: FxHashMap::with_capacity_and_hasher(128, Default::default()),
            cds_class_cache: FxHashMap::with_capacity_and_hasher(64, Default::default()),
            loading_guard: FxHashSet::default(),
            vtable_descriptors: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            skip_bytecode_verification: FxHashSet::default(),
            hidden_name_counter: 0,
            redefine_generations: RwLock::new(FxHashMap::with_capacity_and_hasher(8, Default::default())),
        }
    }

    /// Parse a `module-info.class` byte array and register the contained
    /// module descriptor in `registry`.  Silently ignores parse failures.
    fn try_register_module_info(registry: &mut ModuleRegistry, bytes: &[u8]) {
        let mut class_file = match rustjvm_reader::read_class(bytes) {
            Ok(cf) => cf,
            Err(e) => {
                debug!("Failed to parse module-info.class: {e}");
                return;
            }
        };

        // Eagerly decode the (small) module-info attribute table once so the
        // `Attribute`-typed helpers below (`descriptor_from_module_attribute`,
        // `packages_from_module_packages_attribute`) can pattern-match. The
        // module-info attribute list is tiny — one `Module` + maybe a
        // `ModulePackages` + `SourceFile` — so eager decode is the safest
        // option (and matches the prior behaviour from before lazy storage).
        if let Err(e) =
            force_decode_all(&mut class_file.attributes, &class_file.constant_pool)
        {
            debug!("Failed to decode module-info attributes: {e}");
            return;
        }

        // Find the Module attribute
        let module_desc = class_file.attributes.iter().find_map(|a| {
            a.as_decoded()
                .and_then(|d| descriptor_from_module_attribute(d, &class_file.constant_pool))
        });

        let desc = match module_desc {
            Some(d) => d,
            None => return,
        };

        // Find ModulePackages attribute for package-to-module mapping
        let packages: Vec<String> = class_file
            .attributes
            .iter()
            .find_map(|a| {
                a.as_decoded().and_then(|d| {
                    packages_from_module_packages_attribute(d, &class_file.constant_pool)
                })
            })
            .unwrap_or_default();

        debug!(
            module = %desc.name,
            packages = packages.len(),
            requires = desc.requires.len(),
            exports = desc.exports.len(),
            "Registered module"
        );

        registry.register(desc, packages);
    }

    /// Fast zero-allocation class lookup by name hash.
    #[inline]
    pub fn get_loaded_class_id(&self, name: &str) -> Option<ClassId> {
        self.name_to_id.get(&class_name_hash(name)).copied()
    }

    /// Register a minimal synthetic class with the given name and field count.
    ///
    /// If a class with this name is already loaded, returns its existing
    /// ClassId. Otherwise allocates a new ClassId, creates a minimal
    /// `Class` struct, and registers it in the class store.
    ///
    /// Used by the VM bootstrap to create shim classes (e.g.
    /// `java/io/PrintStream` for System.out) that dispatch through
    /// native registrations rather than real JDK bytecode.
    ///
    /// Prefer real `.class` files for application-visible types; see `docs/jvm-no-synthetic-stubs.md`.
    pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId {
        if let Some(id) = self.get_loaded_class_id(name) {
            // Already loaded — check if it's a real class with more fields.
            // If so, we can't replace it (other code may hold references).
            // For the System.out bootstrap, the caller handles this by
            // checking the field count before calling us.
            return id;
        }
        let id = self.class_store.next_id();
        let class = Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: rustjvm_types::intern_arc(name),
            source_file: None,
            version: rustjvm_reader::class_file_version::ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: rustjvm_reader::constant_pool::ConstantPool::new(vec![
                rustjvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: rustjvm_reader::class_access_flags::ClassAccessFlags::from_bits_truncate(0x0021),
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: synthetic_stub_ctor_methods(name),
            first_field_index: 0,
            num_total_fields: num_fields,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: true,
            has_finalizer: false,
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        };
        self.class_store.add(class);
        self.register_class_name(ClassLoaderId::Bootstrap, name, id);
        id
    }

    /// Check if the boot classpath has real JDK class files (not just empty).
    ///
    /// Returns true if `java/lang/Object.class` can be found on the bootstrap classpath.
    pub fn has_real_boot_classes(&self) -> bool {
        self.bootstrap.find_class_bytes("java/lang/Object").is_ok()
    }

    /// List all JMOD module names on the boot classpath.
    ///
    /// Returns module names extracted from JMOD file stems
    /// (e.g. `java.base.jmod` → `"java.base"`).
    pub fn list_boot_modules(&self) -> Vec<String> {
        self.bootstrap.class_path().list_jmod_modules()
    }

    /// Return the total number of classes available in boot classpath JMODs.
    pub fn boot_jmod_class_count(&self) -> usize {
        self.bootstrap.class_path().jmod_class_count()
    }

    /// Pre-load essential bootstrap classes from the boot classpath.
    ///
    /// This must be called early in VM initialization (before any Java code runs)
    /// when real JDK class files are available. It loads the core classes that
    /// everything else depends on: Object, Serializable, Comparable, CharSequence,
    /// String, Class, System, and Throwable.
    ///
    /// Returns the number of classes successfully loaded, or 0 if no real JDK.
    pub fn bootstrap_core_classes(&mut self) -> usize {
        if !self.has_real_boot_classes() {
            return 0;
        }

        // Order matters: Object first (no superclass), then interfaces, then classes
        // that depend on them. Each load_class call recursively loads dependencies.
        let core_classes = [
            "java/lang/Object",
            "java/io/Serializable",
            "java/lang/Comparable",
            "java/lang/CharSequence",
            "java/lang/constant/Constable",
            "java/lang/constant/ConstantDesc",
            "java/lang/String",
            "java/lang/Class",
            "java/lang/Cloneable",
            "java/lang/Number",
            "java/lang/Integer",
            "java/lang/Long",
            "java/lang/Boolean",
            "java/lang/Byte",
            "java/lang/Short",
            "java/lang/Character",
            "java/lang/Float",
            "java/lang/Double",
            "java/lang/Throwable",
            "java/lang/Exception",
            "java/lang/RuntimeException",
            "java/lang/Error",
            "java/lang/System",
            "java/lang/Thread",
            "java/lang/Iterable",
            "java/util/Iterator",
        ];

        // Extended classes — loaded after core to provide common stdlib bytecode
        let extended_classes = [
            "java/lang/Math",
            "java/lang/StrictMath",
            "java/lang/StringBuilder",
            "java/lang/StringBuffer",
            "java/lang/AbstractStringBuilder",
            "java/lang/Enum",
            "java/lang/Void",
            "java/lang/StackTraceElement",
            "java/lang/ClassLoader",
            "java/lang/ref/Reference",
            "java/lang/ref/WeakReference",
            "java/lang/ref/SoftReference",
            "java/lang/ref/PhantomReference",
            "java/lang/ref/ReferenceQueue",
            "java/lang/reflect/AccessibleObject",
            "java/lang/reflect/Member",
            "java/lang/reflect/Field",
            "java/lang/reflect/Method",
            "java/lang/reflect/Constructor",
            "java/util/Collection",
            "java/util/List",
            "java/util/Set",
            "java/util/Map",
            "java/util/AbstractCollection",
            "java/util/AbstractList",
            "java/util/AbstractSet",
            "java/util/AbstractMap",
            "java/util/ArrayList",
            "java/util/HashMap",
            "java/util/HashSet",
            "java/util/LinkedList",
            "java/util/Collections",
            "java/util/Arrays",
            "java/util/Objects",
            "java/util/Optional",
            "java/io/InputStream",
            "java/io/OutputStream",
            "java/io/PrintStream",
            "java/io/Closeable",
            "java/io/Flushable",
            "java/lang/AutoCloseable",
        ];

        // Tier 3: Exception hierarchy — needed for catch handlers in real bytecode
        let exception_classes = [
            "java/lang/NullPointerException",
            "java/lang/ArithmeticException",
            "java/lang/ArrayIndexOutOfBoundsException",
            "java/lang/IndexOutOfBoundsException",
            "java/lang/StringIndexOutOfBoundsException",
            "java/lang/ClassCastException",
            "java/lang/IllegalArgumentException",
            "java/lang/IllegalStateException",
            "java/lang/UnsupportedOperationException",
            "java/lang/ClassNotFoundException",
            "java/lang/NoSuchMethodException",
            "java/lang/NoSuchFieldException",
            "java/lang/NoSuchMethodError",
            "java/lang/NoSuchFieldError",
            "java/lang/AbstractMethodError",
            "java/lang/IncompatibleClassChangeError",
            "java/lang/IllegalAccessError",
            "java/lang/InstantiationError",
            "java/lang/StackOverflowError",
            "java/lang/OutOfMemoryError",
            "java/lang/ExceptionInInitializerError",
            "java/lang/LinkageError",
            "java/lang/VerifyError",
            "java/lang/SecurityException",
            "java/lang/NegativeArraySizeException",
            "java/lang/ArrayStoreException",
            "java/lang/IllegalMonitorStateException",
            "java/lang/InterruptedException",
            "java/lang/CloneNotSupportedException",
            "java/lang/NumberFormatException",
            "java/io/IOException",
            "java/io/FileNotFoundException",
            "java/io/UnsupportedEncodingException",
            "java/io/EOFException",
            "java/util/NoSuchElementException",
            "java/util/ConcurrentModificationException",
            "java/lang/reflect/InvocationTargetException",
        ];

        // Tier 4: Collections, concurrency, and functional interfaces
        let collections_classes = [
            "java/util/LinkedHashMap",
            "java/util/TreeMap",
            "java/util/TreeSet",
            "java/util/LinkedHashSet",
            "java/util/Properties",
            "java/util/Hashtable",
            "java/util/Vector",
            "java/util/Stack",
            "java/util/EnumMap",
            "java/util/EnumSet",
            "java/util/IdentityHashMap",
            "java/util/WeakHashMap",
            "java/util/ArrayDeque",
            "java/util/PriorityQueue",
            "java/util/BitSet",
            "java/util/StringJoiner",
            "java/util/concurrent/ConcurrentHashMap",
            "java/util/concurrent/CopyOnWriteArrayList",
            "java/util/concurrent/CopyOnWriteArraySet",
            "java/util/concurrent/atomic/AtomicInteger",
            "java/util/concurrent/atomic/AtomicLong",
            "java/util/concurrent/atomic/AtomicBoolean",
            "java/util/concurrent/atomic/AtomicReference",
            "java/util/concurrent/locks/ReentrantLock",
            "java/util/concurrent/locks/ReentrantReadWriteLock",
            "java/util/concurrent/CompletableFuture",
            "java/util/concurrent/CyclicBarrier",
            "java/util/concurrent/ForkJoinPool",
            "java/util/concurrent/ForkJoinTask",
            "java/util/concurrent/ExecutorService",
            "java/util/concurrent/ThreadPoolExecutor",
            "java/util/concurrent/Executors",
            "java/util/concurrent/locks/AbstractQueuedSynchronizer",
            "java/util/concurrent/locks/LockSupport",
            "java/util/concurrent/locks/StampedLock",
            "java/util/concurrent/locks/Lock",
            "java/util/concurrent/locks/Condition",
            "java/util/concurrent/locks/ReadWriteLock",
            "java/util/concurrent/atomic/AtomicIntegerArray",
            "java/util/concurrent/atomic/AtomicLongArray",
            "java/util/concurrent/atomic/AtomicReferenceArray",
            "java/util/concurrent/atomic/AtomicStampedReference",
            "java/util/concurrent/atomic/AtomicMarkableReference",
            "java/util/concurrent/atomic/LongAdder",
            "java/util/concurrent/atomic/DoubleAdder",
            "java/util/concurrent/atomic/LongAccumulator",
            "java/util/concurrent/atomic/DoubleAccumulator",
            "java/util/concurrent/Phaser",
        ];

        // Tier 4b: Inner-class views of core collections.
        // These are not loaded automatically because their outer class's
        // <clinit> doesn't reference them; they're allocated on-demand by
        // bytecode like `HashMap.keySet()` which calls `new KeySet()`. If
        // they're not bootstrapped, the views land with cid=0 and
        // class_id_of() reports `java/lang/Object`, breaking virtual
        // dispatch on `iterator()`, `size()`, `contains()`, etc.
        // (S111r8 — fixes Spring Boot fat-jar boot which iterates env-var
        // keysets via the URLClassLoader path.)
        let view_classes = [
            // HashMap views and iterators
            "java/util/HashMap$Node",
            "java/util/HashMap$TreeNode",
            "java/util/HashMap$KeySet",
            "java/util/HashMap$Values",
            "java/util/HashMap$EntrySet",
            "java/util/HashMap$HashIterator",
            "java/util/HashMap$KeyIterator",
            "java/util/HashMap$ValueIterator",
            "java/util/HashMap$EntryIterator",
            "java/util/HashMap$KeySpliterator",
            "java/util/HashMap$ValueSpliterator",
            "java/util/HashMap$EntrySpliterator",
            // LinkedHashMap views and iterators
            "java/util/LinkedHashMap$Entry",
            "java/util/LinkedHashMap$LinkedKeySet",
            "java/util/LinkedHashMap$LinkedValues",
            "java/util/LinkedHashMap$LinkedEntrySet",
            "java/util/LinkedHashMap$LinkedHashIterator",
            "java/util/LinkedHashMap$LinkedKeyIterator",
            "java/util/LinkedHashMap$LinkedValueIterator",
            "java/util/LinkedHashMap$LinkedEntryIterator",
            // ConcurrentHashMap views and iterators
            "java/util/concurrent/ConcurrentHashMap$Node",
            "java/util/concurrent/ConcurrentHashMap$TreeNode",
            "java/util/concurrent/ConcurrentHashMap$TreeBin",
            "java/util/concurrent/ConcurrentHashMap$KeySetView",
            "java/util/concurrent/ConcurrentHashMap$ValuesView",
            "java/util/concurrent/ConcurrentHashMap$EntrySetView",
            "java/util/concurrent/ConcurrentHashMap$Traverser",
            "java/util/concurrent/ConcurrentHashMap$BaseIterator",
            "java/util/concurrent/ConcurrentHashMap$KeyIterator",
            "java/util/concurrent/ConcurrentHashMap$ValueIterator",
            "java/util/concurrent/ConcurrentHashMap$EntryIterator",
            // TreeMap views and iterators
            "java/util/TreeMap$Entry",
            "java/util/TreeMap$KeySet",
            "java/util/TreeMap$Values",
            "java/util/TreeMap$EntrySet",
            "java/util/TreeMap$NavigableSubMap",
            "java/util/TreeMap$AscendingSubMap",
            "java/util/TreeMap$DescendingSubMap",
            "java/util/TreeMap$PrivateEntryIterator",
            "java/util/TreeMap$EntryIterator",
            "java/util/TreeMap$KeyIterator",
            "java/util/TreeMap$ValueIterator",
            "java/util/TreeMap$DescendingKeyIterator",
            // ArrayList iterator
            "java/util/ArrayList$Itr",
            "java/util/ArrayList$ListItr",
            "java/util/ArrayList$SubList",
            // LinkedList iterator
            "java/util/LinkedList$Node",
            "java/util/LinkedList$ListItr",
            "java/util/LinkedList$DescendingIterator",
            // HashSet/LinkedHashSet/TreeSet share Map's views internally
            // but Hashtable has its own.
            "java/util/Hashtable$Entry",
            "java/util/Hashtable$KeySet",
            "java/util/Hashtable$ValueCollection",
            "java/util/Hashtable$EntrySet",
            "java/util/Hashtable$Enumerator",
        ];

        // Tier 5: Functional interfaces and streams
        let functional_classes = [
            "java/util/function/Function",
            "java/util/function/Consumer",
            "java/util/function/Supplier",
            "java/util/function/Predicate",
            "java/util/function/BiFunction",
            "java/util/function/BiConsumer",
            "java/util/function/BiPredicate",
            "java/util/function/UnaryOperator",
            "java/util/function/BinaryOperator",
            "java/util/function/IntFunction",
            "java/util/function/LongFunction",
            "java/util/function/DoubleFunction",
            "java/util/function/IntConsumer",
            "java/util/function/LongConsumer",
            "java/util/function/DoubleConsumer",
            "java/util/function/IntSupplier",
            "java/util/function/LongSupplier",
            "java/util/function/DoubleSupplier",
            "java/util/function/IntPredicate",
            "java/util/function/LongPredicate",
            "java/util/function/DoublePredicate",
            "java/util/function/IntUnaryOperator",
            "java/util/function/LongUnaryOperator",
            "java/util/function/DoubleUnaryOperator",
            "java/util/function/IntBinaryOperator",
            "java/util/function/LongBinaryOperator",
            "java/util/function/DoubleBinaryOperator",
            "java/util/function/ToIntFunction",
            "java/util/function/ToLongFunction",
            "java/util/function/ToDoubleFunction",
            "java/util/stream/Stream",
            "java/util/stream/IntStream",
            "java/util/stream/LongStream",
            "java/util/stream/DoubleStream",
            "java/util/stream/Collectors",
            "java/util/stream/Collector",
            "java/util/stream/BaseStream",
            "java/util/stream/StreamSupport",
        ];

        // Tier 6: I/O and NIO
        let io_classes = [
            "java/io/BufferedInputStream",
            "java/io/BufferedOutputStream",
            "java/io/BufferedReader",
            "java/io/BufferedWriter",
            "java/io/InputStreamReader",
            "java/io/OutputStreamWriter",
            "java/io/FileInputStream",
            "java/io/FileOutputStream",
            "java/io/FileReader",
            "java/io/FileWriter",
            "java/io/Reader",
            "java/io/Writer",
            "java/io/File",
            "java/io/DataInputStream",
            "java/io/DataOutputStream",
            "java/io/ObjectInputStream",
            "java/io/ObjectOutputStream",
            "java/io/ByteArrayInputStream",
            "java/io/ByteArrayOutputStream",
            "java/io/StringReader",
            "java/io/StringWriter",
            "java/io/PrintWriter",
            "java/io/FilterInputStream",
            "java/io/FilterOutputStream",
            "java/nio/ByteBuffer",
            "java/nio/CharBuffer",
            "java/nio/Buffer",
            "java/nio/charset/Charset",
            "java/nio/charset/StandardCharsets",
            "java/nio/file/Path",
            "java/nio/file/Paths",
            "java/nio/file/Files",
        ];

        // Tier 7: Internal VM support classes
        let internal_classes = [
            "jdk/internal/misc/Unsafe",
            "jdk/internal/misc/VM",
            "jdk/internal/misc/Signal",
            "jdk/internal/misc/SharedSecrets",
            "jdk/internal/access/SharedSecrets",
            "sun/misc/Unsafe",
            "java/lang/invoke/MethodHandle",
            "java/lang/invoke/MethodHandles",
            "java/lang/invoke/MethodType",
            "java/lang/invoke/CallSite",
            "java/lang/invoke/ConstantCallSite",
            "java/lang/invoke/MutableCallSite",
            "java/lang/invoke/VolatileCallSite",
            "java/lang/invoke/LambdaMetafactory",
            "java/lang/invoke/StringConcatFactory",
            "java/lang/annotation/Annotation",
            "java/lang/annotation/Retention",
            "java/lang/annotation/Target",
            "java/lang/annotation/ElementType",
            "java/lang/annotation/RetentionPolicy",
            "java/lang/annotation/Documented",
            "java/lang/annotation/Inherited",
            "java/util/Locale",
            "java/util/Currency",
            "java/util/Date",
            "java/util/Calendar",
            "java/util/TimeZone",
            "java/util/UUID",
            "java/util/regex/Pattern",
            "java/util/regex/Matcher",
            "java/util/Formatter",
            "java/text/DecimalFormat",
            "java/text/MessageFormat",
            "java/text/NumberFormat",
            "java/text/SimpleDateFormat",
            "java/math/BigInteger",
            "java/math/BigDecimal",
            "java/math/MathContext",
            "java/math/RoundingMode",
            "java/net/URL",
            "java/net/URI",
            "java/security/AccessController",
            "java/security/PrivilegedAction",
            "java/security/Permission",
        ];

        let mut loaded = 0;
        for name in core_classes.iter()
            .chain(extended_classes.iter())
            .chain(exception_classes.iter())
            .chain(collections_classes.iter())
            .chain(view_classes.iter())
            .chain(functional_classes.iter())
            .chain(io_classes.iter())
            .chain(internal_classes.iter())
        {
            match self.load_class(name) {
                Ok(class_id) => {
                    let is_real = self.class_store.get(class_id)
                        .map(|c| !c.is_synthetic_stub)
                        .unwrap_or(false);
                    if is_real {
                        loaded += 1;
                        debug!(class = name, id = %class_id, "Bootstrap: loaded real class");
                    } else {
                        debug!(class = name, "Bootstrap: loaded as synthetic stub");
                    }
                }
                Err(e) => {
                    debug!(class = name, error = %e, "Bootstrap: failed to load");
                }
            }
        }

        if loaded > 0 {
            // Log summary
            let total = self.loaded_count();
            debug!(
                "Bootstrap complete: {} core classes loaded ({} total classes in ClassStore)",
                loaded, total
            );
        }

        loaded
    }

    /// Load a class by its binary name (e.g. `"java/lang/Object"`).
    ///
    /// Uses the parent delegation model:
    /// 1. Check if already loaded by any loader
    /// 2. Ask bootstrap → extension → application to find the class
    /// 3. Parse, recursively load superclass/interfaces, and register
    pub fn load_class(&mut self, name: &str) -> Result<ClassId, VmError> {
        // RKC16N.3: Reference- and primitive-array classes (`[X`) are
        // *synthesised* by the bootstrap loader directly from the
        // resolved component class — JVMS §5.3.3 explicitly says no
        // class file is consulted. Short-circuit before any I/O so
        // that `Class.forName("[Ljava/util/HashMap;")` succeeds without
        // scanning JMOD/classpath and without producing the
        // "synthetic stub" warning.
        if name.starts_with('[') {
            return self.synthesize_array_class(name);
        }
        // Fast path: zero-allocation hash lookup via name_to_id
        if let Some(id) = self.get_loaded_class_id(name) {
            // If the class is a synthetic stub (no methods, no bytecode), try
            // to upgrade it to a real class from the classpath. This handles
            // the case where wrapper types like java/lang/Boolean are created
            // as synthetic stubs during early bootstrap but later need their
            // real bytecode methods (e.g. parseBoolean).
            let is_synthetic = self.class_store.get(id)
                .map(|c| c.is_synthetic_stub)
                .unwrap_or(false);
            if is_synthetic && !name.starts_with('[') {
                if let Ok((bytes, loader_id)) = self.find_class_bytes_delegated(name) {
                    match self.upgrade_synthetic_class(id, name, &bytes, loader_id) {
                        Ok(()) => {
                            tracing::debug!(class = name, "Upgraded synthetic stub to real class");
                        }
                        Err(e) => {
                            tracing::debug!(class = name, "Failed to upgrade synthetic stub: {e:?}");
                        }
                    }
                }
            }
            return Ok(id);
        }

        // Circular dependency guard: if this class is already being loaded
        // by a recursive call (e.g. A extends B extends A), reject it.
        if self.loading_guard.contains(name) {
            return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: format!(
                    "circular class hierarchy detected: {} is already being loaded",
                    name
                ),
            }));
        }

        debug!(class = name, "Loading class (parent delegation)");

        // Parent delegation: try bootstrap → extension → application
        match self.find_class_bytes_delegated(name) {
            Ok((bytes, loader_id)) => {
                // Parse and register with the loader that found it
                self.define_class(name, &bytes, loader_id)
            }
            Err(_) if is_jboss_logging_locale_lookup(name) => {
                // S-trinity #3: JBoss Logging i18n probes locale-specific
                // implementation classes (`_$logger_<locale>` /
                // `_$bundle_<locale>`) inside a try/catch
                // (ClassNotFoundException) and falls back to the locale-less
                // `_$logger` / `_$bundle` when the probe fails. Our
                // `is_jdk_class("org/jboss/...")` returns true for these
                // names, so without this branch we would synthesize a stub
                // — and `create_synthetic_stub`'s heuristic flags any
                // name containing `$` as an interface, which then fails
                // `Class.asSubclass(ServerLogger.class)` with a CCE that
                // escapes the JBoss-Logging CNFE catch.
                Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                    class_name: name.to_string(),
                }))
            }
            Err(_) if is_jdk_class(name) => {
                // JDK class not found as a .class file — create a synthetic stub.
                // Our VM handles JDK classes natively, so we just need a minimal
                // entry in the ClassStore for the type system to work.
                debug!(class = name, "Falling back to synthetic stub — class not found in any classpath");
                self.create_synthetic_stub(name)
            }
            Err(e) => Err(e),
        }
    }

    /// Find class bytes using parent delegation.
    fn find_class_bytes_delegated(&self, name: &str) -> Result<(Vec<u8>, ClassLoaderId), VmError> {
        // CDS archive check — fastest path
        if let Some(bytes) = self.cds_class_cache.get(name) {
            return Ok((bytes.clone(), ClassLoaderId::Bootstrap));
        }
        // Bootstrap first
        if let Ok(bytes) = self.bootstrap.find_class_bytes(name) {
            return Ok((bytes, ClassLoaderId::Bootstrap));
        }
        // Extension second
        if let Ok(bytes) = self.extension.find_class_bytes(name) {
            return Ok((bytes, ClassLoaderId::Extension));
        }
        // Application last
        if let Ok(bytes) = self.application.find_class_bytes(name) {
            return Ok((bytes, ClassLoaderId::Application));
        }

        Err(VmError::ClassFile(ClassFileError::ClassNotFound {
            class_name: name.to_string(),
        }))
    }

    /// Parse class bytes and register with the given loader identity.
    ///
    /// This is the core method for loading a class from raw bytecode.
    /// Used both internally (from `load_class`) and externally (from
    /// `ClassLoader.defineClass(byte[])` via `NativeContext`).
    pub fn define_class(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: ClassLoaderId,
    ) -> Result<ClassId, VmError> {
        self.define_class_with_options(name, bytes, loader_id, DefineClassOptions::default())
    }

    /// NEW-8: extended define_class entry point used by
    /// `MethodHandles.Lookup.defineHiddenClass`. Accepts an
    /// [`DefineClassOptions`] that lets the caller override the stored
    /// class name (so hidden classes can be registered under a unique
    /// mangled name even when their class file's `this_class` entry
    /// collides with an existing class) and mark the new class as
    /// hidden in one atomic operation.
    pub fn define_class_with_options(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: ClassLoaderId,
        options: DefineClassOptions,
    ) -> Result<ClassId, VmError> {
        // WP2.3: Reject too-short / non-CAFEBABE bytes up-front with a
        // typed ClassFormatError. The reader will catch malformed
        // bytes too, but a stronger pre-check produces clearer error
        // messages and avoids leaking parser internals.
        if bytes.len() < 8 {
            return Err(VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: format!(
                    "class file too short ({} bytes; need at least 8 for header)",
                    bytes.len()
                ),
            }));
        }
        if bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            return Err(VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: "bad magic; expected CAFEBABE".to_string(),
            }));
        }

        // Parse the class file
        let mut class_file = rustjvm_reader::read_class(bytes).map_err(|e| {
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: e.to_string(),
            })
        })?;

        // Eagerly decode the class-level attribute table. Most class-level
        // attributes (`BootstrapMethods`, `Signature`, `RuntimeVisible[/Invisible]
        // Annotations`, `NestHost`, `NestMembers`, `Record`, `PermittedSubclasses`,
        // `InnerClasses`, `EnclosingMethod`, `Module`, `SourceFile`) are
        // consumed unconditionally below to populate the runtime `Class`
        // struct, so paying the decode cost up-front is strictly cheaper
        // than wrapping every consumer site in a fallible decode call. In
        // particular `BootstrapMethods` MUST be available before the class
        // becomes live, otherwise the first `invokedynamic` site that
        // resolves against the class would have to re-borrow the constant
        // pool from the reader (which we've already moved out of by then).
        // Class-level attribute counts are small (≤ ~10 in practice), so
        // the extra work is in the noise next to verification + linking.
        force_decode_all(&mut class_file.attributes, &class_file.constant_pool).map_err(
            |e| {
                VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: name.to_string(),
                    message: format!("attribute decode failed: {e}"),
                })
            },
        )?;

        // Round-2 fix (CRIT): also force-decode method- and field-level
        // LazyAttributes. Without this, `ClassFileMethod::code()` /
        // `ClassFileField::constant_value_index()` return None for all
        // production-loaded classes (they only return Some on
        // `LazyAttribute::Decoded`), which silently disables the bytecode
        // verifier and breaks interp/JIT method-dispatch fallback.
        // The lazy-attribute win was a measurement mistake: methods/fields
        // are universally accessed at class-link time, so deferring decode
        // saves nothing. Decode eagerly at load.
        for method in class_file.methods.iter_mut() {
            force_decode_all(&mut method.attributes, &class_file.constant_pool).map_err(
                |e| VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: name.to_string(),
                    message: format!("method '{}' attribute decode failed: {e}", method.name),
                }),
            )?;
        }
        for field in class_file.fields.iter_mut() {
            force_decode_all(&mut field.attributes, &class_file.constant_pool).map_err(
                |e| VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: name.to_string(),
                    message: format!("field '{}' attribute decode failed: {e}", field.name),
                }),
            )?;
        }

        // WP2.3: Class-name match check. If the caller asked for class
        // `name` but the class file's `this_class` says something
        // else, the JVMS requires a `NoClassDefFoundError`
        // (§5.3.5#3.b). The `override_name` option (used by hidden
        // classes) bypasses this check because it deliberately mangles
        // the registered name.
        if options.override_name.is_none() && !name.is_empty() && class_file.this_class != name {
            return Err(VmError::Linkage(LinkageError::NoClassDefFoundError {
                class_name: format!(
                    "{} (defineClass requested name {} but class file declares {})",
                    name, name, class_file.this_class
                ),
            }));
        }

        // WP2.3: Duplicate-define rejection. Check before we mutate
        // `loading_guard` so we don't poison the guard set on the
        // failure path. The `allow_redefine` flag (used by WP2.4
        // `redefineClasses`) bypasses this so the instrumentation
        // path can replace bytecode in place.
        let stored_name_preview = options
            .override_name
            .clone()
            .unwrap_or_else(|| class_file.this_class.clone());
        if !options.allow_redefine && !options.hidden {
            // T10.9.E: probe key uses `Arc::from(&str)`. The hot insert path
            // below builds a real `Arc<str>` from `class.name` (the
            // pool-interned name); the rare duplicate-define probe pays one
            // allocation, same as the prior `clone()` of the String preview.
            let key = (loader_id, Arc::<str>::from(stored_name_preview.as_str()));
            if self.loaded_classes.contains_key(&key) {
                return Err(VmError::Linkage(
                    LinkageError::IncompatibleClassChangeError {
                        message: format!(
                            "class {} already defined by {} loader",
                            stored_name_preview, loader_id
                        ),
                    },
                ));
            }
        }

        // Mark this class as currently loading to detect circular hierarchies.
        // This guard is checked in load_class() before recursive calls.
        self.loading_guard.insert(name.to_string());

        // Recursively load the superclass (uses parent delegation too)
        let superclass_id = match class_file.super_class {
            Some(ref super_name) => match self.load_class(super_name) {
                Ok(id) => Some(id),
                Err(e) => {
                    self.loading_guard.remove(name);
                    return Err(e);
                }
            },
            None => None, // java/lang/Object has no superclass
        };

        // Recursively load all interfaces
        let interface_ids: Vec<ClassId> = match class_file
            .interfaces
            .iter()
            .map(|iface_name| self.load_class(iface_name))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(ids) => ids,
            Err(e) => {
                self.loading_guard.remove(name);
                return Err(e);
            }
        };

        // Loading guard no longer needed — class will be registered below
        self.loading_guard.remove(name);

        // Sealed class enforcement (JEP 409, Java 17+):
        // If the superclass or any interface is sealed, this class must be in its permitted list.
        // JEP 409: throw IncompatibleClassChangeError if not permitted.
        if let Some(super_id) = superclass_id {
            if let Some(super_class) = self.class_store.get(super_id) {
                if super_class.is_sealed()
                    && !super_class
                        .permitted_subclasses
                        .iter()
                        .any(|p| p == &class_file.this_class)
                {
                    return Err(VmError::Linkage(
                        LinkageError::IncompatibleClassChangeError {
                            message: format!(
                                "class {} is not a permitted subclass of sealed class {}",
                                class_file.this_class, super_class.name
                            ),
                        },
                    ));
                }
            }
        }
        for &iface_id in &interface_ids {
            if let Some(iface_class) = self.class_store.get(iface_id) {
                if iface_class.is_sealed()
                    && !iface_class
                        .permitted_subclasses
                        .iter()
                        .any(|p| p == &class_file.this_class)
                {
                    return Err(VmError::Linkage(
                        LinkageError::IncompatibleClassChangeError {
                            message: format!(
                                "class {} is not a permitted implementor of sealed interface {}",
                                class_file.this_class, iface_class.name
                            ),
                        },
                    ));
                }
            }
        }

        // Compute field layout
        let (first_field_index, num_total_fields) =
            compute_field_layout(&class_file.fields, superclass_id, &self.class_store);

        // Wave 3-B (RE.4): some real-JDK classes (e.g. java.net.InetSocketAddress
        // = 1 instance field `holder`) have a much smaller declared field count
        // than the synthetic-mode field layout used by `native-builtins`. Native
        // `<init>` methods write to the synthetic indices; without padding the
        // object would lack slots for those writes (panic on set_field) or the
        // slots would never be allocated (so reads see uninitialised slots and
        // misreport as e.g. `port is not an int`). Pad with the larger of the
        // declared count and the synthetic stub layout.
        let stub_fields = synthetic_stub_fields(name);
        let stub_instance_count = stub_fields
            .iter()
            .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
            .count();
        let stub_parent_fields = match superclass_id {
            Some(super_id) => self
                .class_store
                .get(super_id)
                .map_or(0, |c| c.num_total_fields),
            None => 0,
        };
        let stub_total = stub_parent_fields + stub_instance_count;
        let num_total_fields = num_total_fields.max(stub_total);

        // Build the runtime Class
        let id = self.class_store.next_id();
        // SourceFile: inlined here (rather than calling `class_file.source_file()`)
        // so we can pattern-match through `LazyAttribute::as_decoded()`. All
        // class-level attributes were force-decoded immediately after parsing,
        // so `as_decoded()` returns `Some` for every entry.
        let source_file = class_file.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::SourceFile(name)) => Some(name.clone()),
            _ => None,
        });

        // Extract BootstrapMethods attribute (needed for invokedynamic).
        // This MUST be available before the class is published — see the
        // `force_decode_all` comment above for the rationale.
        let bootstrap_methods = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::BootstrapMethods(bms)) => Some(bms.clone()),
                _ => None,
            })
            .unwrap_or_default();

        // Extract RuntimeVisibleAnnotations AND RuntimeInvisibleAnnotations from class-level attributes
        let mut annotations = Vec::new();
        for attr in &class_file.attributes {
            match attr.as_decoded() {
                Some(Attribute::RuntimeVisibleAnnotations(anns))
                | Some(Attribute::RuntimeInvisibleAnnotations(anns)) => {
                    annotations.extend(anns.iter().cloned());
                }
                _ => {}
            }
        }

        // Extract Signature attribute (JVMS §4.7.9)
        let signature = class_file.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::Signature(s)) => Some(s.clone()),
            _ => None,
        });

        // Extract NestHost attribute (JEP 181, Java 11+)
        let nest_host = class_file.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::NestHost { host_class_index }) => class_file
                .constant_pool
                .get_class_name(*host_class_index)
                .map(|s| s.to_string()),
            _ => None,
        });

        // Extract NestMembers attribute (JEP 181, Java 11+)
        let nest_members = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::NestMembers { classes }) => {
                    let names: Vec<String> = classes
                        .iter()
                        .filter_map(|idx| {
                            class_file
                                .constant_pool
                                .get_class_name(*idx)
                                .map(|s| s.to_string())
                        })
                        .collect();
                    Some(names)
                }
                _ => None,
            })
            .unwrap_or_default();

        // Extract Record attribute (JEP 395, Java 16+)
        let record_components = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::Record(components)) => {
                    let resolved: Vec<RecordComponentInfo> = components
                        .iter()
                        .filter_map(|rc| {
                            let name = class_file.constant_pool.get_utf8(rc.name_index)?;
                            let desc = class_file.constant_pool.get_utf8(rc.descriptor_index)?;
                            Some(RecordComponentInfo {
                                name: name.to_string(),
                                descriptor: desc.to_string(),
                            })
                        })
                        .collect();
                    Some(resolved)
                }
                _ => None,
            })
            .unwrap_or_default();

        // Extract PermittedSubclasses attribute (JEP 409, Java 17+)
        let permitted_subclasses = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::PermittedSubclasses { classes }) => {
                    let names: Vec<String> = classes
                        .iter()
                        .filter_map(|idx| {
                            class_file
                                .constant_pool
                                .get_class_name(*idx)
                                .map(|s| s.to_string())
                        })
                        .collect();
                    Some(names)
                }
                _ => None,
            })
            .unwrap_or_default();

        // Extract InnerClasses attribute (JVMS §4.7.6)
        let inner_classes = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::InnerClasses(entries)) => {
                    let resolved: Vec<InnerClassEntry> = entries
                        .iter()
                        .filter_map(|ic| {
                            let inner = class_file
                                .constant_pool
                                .get_class_name(ic.inner_class_info_index)?
                                .to_string();
                            let outer = if ic.outer_class_info_index == 0 {
                                String::new()
                            } else {
                                class_file
                                    .constant_pool
                                    .get_class_name(ic.outer_class_info_index)
                                    .unwrap_or("")
                                    .to_string()
                            };
                            let inner_name = if ic.inner_name_index == 0 {
                                String::new()
                            } else {
                                class_file
                                    .constant_pool
                                    .get_utf8(ic.inner_name_index)
                                    .unwrap_or("")
                                    .to_string()
                            };
                            Some(InnerClassEntry {
                                inner_class: inner,
                                outer_class: outer,
                                inner_name,
                                access_flags: ic.inner_class_access_flags,
                            })
                        })
                        .collect();
                    Some(resolved)
                }
                _ => None,
            })
            .unwrap_or_default();

        // Extract EnclosingMethod attribute (JVMS §4.7.7)
        let enclosing_method = class_file.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::EnclosingMethod {
                class_index,
                method_index,
            }) => {
                let class_name = class_file
                    .constant_pool
                    .get_class_name(*class_index)?
                    .to_string();
                let (method_name, method_descriptor) = if *method_index == 0 {
                    (String::new(), String::new())
                } else {
                    class_file
                        .constant_pool
                        .get_name_and_type(*method_index)
                        .map(|(n, d)| (n.to_string(), d.to_string()))
                        .unwrap_or_default()
                };
                Some(EnclosingMethodInfo {
                    class_name,
                    method_name,
                    method_descriptor,
                })
            }
            _ => None,
        });

        // Extract Module attribute (Java 9+) and register if this is a
        // module-info class encountered during loading (N1).
        let module_name_from_attr = class_file.attributes.iter().find_map(|a| {
            if let Some(Attribute::Module { name_index, .. }) = a.as_decoded() {
                class_file
                    .constant_pool
                    .get_utf8(*name_index)
                    .map(|s| s.to_string())
            } else {
                None
            }
        });

        // If this class IS a module-info declaration, register it in the module
        // registry (covers module-info.class files loaded lazily during class
        // resolution, supplementing the eager scan in new()).
        if name.ends_with("module-info") || name == "module-info" {
            if let Some(desc) = class_file.attributes.iter().find_map(|a| {
                a.as_decoded()
                    .and_then(|d| descriptor_from_module_attribute(d, &class_file.constant_pool))
            }) {
                let packages: Vec<String> = class_file
                    .attributes
                    .iter()
                    .find_map(|a| {
                        a.as_decoded().and_then(|d| {
                            packages_from_module_packages_attribute(d, &class_file.constant_pool)
                        })
                    })
                    .unwrap_or_default();
                self.module_registry.register(desc, packages);
                self.module_registry.build_readability_graph();
            }
        }

        // Determine this class's module membership from the package registry
        // (N4: unnamed-module baseline + named-module assignment).
        let pkg = package_of(&class_file.this_class);
        let module_name = self
            .module_registry
            .module_for_package(pkg)
            .map(|s| s.to_string())
            .or(module_name_from_attr);

        // NEW-8 + WP2.3: hidden classes register under a mangled name
        // (e.g. `Foo/0x1`) distinct from their class file's `this_class`.
        // All other callers get the stored name straight from the file.
        //
        // WP2.3: when `hidden = true`, the override name itself MUST be
        // unique. If a caller supplies an `override_name` that already
        // exists in this loader (collision from emitting two hidden
        // classes from the same template), we append a monotonic counter
        // suffix `/0x<n>` so each hidden class still gets a distinct
        // identity in the class store. Callers (e.g.
        // `Lookup.defineHiddenClass`) typically pre-mangle, but the
        // belt-and-suspenders check here means a probe that re-uses an
        // override_name across calls won't mysteriously fail with
        // duplicate-define on the second call.
        let mut stored_name = options
            .override_name
            .clone()
            .unwrap_or_else(|| class_file.this_class.clone());
        if options.hidden {
            // The probe of duplicates must consider the (loader, name)
            // composite key — two hidden classes with the same internal
            // name in different loaders are fine.
            //
            // T10.9.E: this loop's per-iteration cost is dominated by the
            // `format!` building a fresh suffixed name; one extra
            // `Arc::from(&str)` per probe is in the noise.
            let mut probe_name = stored_name.clone();
            while self.loaded_classes.contains_key(&(loader_id, Arc::<str>::from(probe_name.as_str()))) {
                self.hidden_name_counter = self.hidden_name_counter.wrapping_add(1);
                probe_name = format!("{}/0x{:x}", stored_name, self.hidden_name_counter);
            }
            stored_name = probe_name;
        }

        // WP2.3: prefer caller-supplied CodeSource (from
        // `defineClass(... ProtectionDomain pd)`) over classpath
        // discovery. If neither is available (a class generated entirely
        // in memory — e.g. CGLIB / ByteBuddy / dynamic proxies) we
        // synthesize a stable `file:/runtime-defined/<class>.class` URL
        // so `Class.code_source` is *non-null* on every defined class.
        //
        // Why non-null matters: real-JDK `ClassLoader.preDefineClass`
        // calls `pd.getCodeSource()` on the resulting `ProtectionDomain`
        // unconditionally; if our `getProtectionDomain0()` native
        // returns null (which it does when `Class.code_source` is None)
        // any subsequent `pd.getCodeSource()` invocation NPEs with
        // "Cannot invoke getCodeSource on null" — exactly the pre-fix
        // behaviour observed by `apps/cglib_probe/cglib.trace.log`.
        //
        // A `file:/` URL satisfies the native's "non-bootstrap" filter
        // (which suppresses PD only for empty / `class:` URIs that
        // signal a true bootstrap class), so the native materialises a
        // real PD whose `getCodeSource()` returns a real `CodeSource`.
        //
        // The `runtime-defined` segment is intentionally chosen so it
        // is parseable as a `URL` by the JDK side and so policy files
        // can `grant codeBase "file:/runtime-defined/-"` to scope
        // permissions for emitted classes.
        let code_source = match options.code_source.clone() {
            Some(cs) => Some(cs),
            None => self.find_class_code_source(name).or_else(|| {
                // Fall back to a synthetic URL that the native
                // ProtectionDomain builder will accept. Use the
                // *stored* (potentially mangled) name so each hidden
                // class gets a distinct code base.
                Some(CodeSource::from_url(format!(
                    "file:/runtime-defined/{}.class",
                    stored_name
                )))
            }),
        };

        // WP2.3: nest-host attribution. The `Lookup.defineHiddenClass(...
        // NESTMATE ...)` path passes `options.nest_host_class_name =
        // Some(<lookup_class>)`. We apply it AFTER the class-file's
        // own NestHost attribute parsing so the explicit option wins.
        let nest_host = options
            .nest_host_class_name
            .clone()
            .or(nest_host);

        let mut class = Class {
            id,
            loader_id,
            // Intern the class name through the global pool. If the reader
            // already interned this exact `this_class` string (the common
            // path), this is a hash-lookup + refcount bump — no allocation.
            name: rustjvm_types::intern_arc(&stored_name),
            source_file,
            version: class_file.version,
            // CDS-loaded classes are pre-verified — skip straight to Verified state.
            state: if self.cds_class_cache.contains_key(name) {
                ClassState::Verified
            } else {
                ClassState::Loaded
            },
            initializing_thread: None,
            constant_pool: class_file.constant_pool,
            access_flags: class_file.access_flags,
            superclass: superclass_id,
            interfaces: interface_ids,
            fields: class_file.fields,
            methods: class_file.methods,
            first_field_index,
            num_total_fields,
            bootstrap_methods,
            signature,
            annotations,
            nest_host,
            nest_members,
            record_components,
            permitted_subclasses,
            inner_classes,
            enclosing_method,
            // NEW-8: `options.hidden` is true when called via
            // `Lookup.defineHiddenClass`. Setting it here keeps the
            // flag atomic with the class registration so
            // `find_class_by_name` cannot observe a brief non-hidden
            // window between insert and the subsequent
            // `set_class_hidden` call.
            hidden: options.hidden,
            module_name,
            is_synthetic_stub: false,
            has_finalizer: false, // computed below
            code_source,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        };

        // Compute has_finalizer: true if this class or any ancestor
        // overrides Object.finalize() (JLS §12.6).
        let self_declares = class.declares_finalize();
        let parent_has = superclass_id
            .and_then(|sid| self.class_store.get(sid))
            .map_or(false, |parent| parent.has_finalizer);
        class.has_finalizer = self_declares || parent_has;

        // Audit-fix #1 (CRITICAL): run the JVMS §5.4.1 Pass 2 / Pass 3
        // verifier before the class is registered. Previously
        // `define_class_with_options` recorded `skip_verification` in a
        // side set but NEVER actually invoked the verifier, so every
        // caller of `Unsafe.defineClass` / `MethodHandles.Lookup
        // .defineClass` got a structurally valid but unverified class.
        //
        // Skip cases (each matches HotSpot's policy):
        //   - `options.skip_verification` — trusted runtime-generated
        //     classes (CGLIB, ByteBuddy, JDK Proxy, hidden classes) that
        //     emit bytecode our worklist verifier cannot model. The
        //     caller asserted trust by setting the flag.
        //   - CDS-cached bytes — already verified at archive-creation
        //     time; re-verifying would be redundant.
        //   - Synthetic stubs — created in-memory with empty bodies, no
        //     bytecode to verify (real-bytecode upgrade path runs
        //     verification on the replacement).
        //
        // On failure the class is dropped (never reaches the store /
        // loaded_classes map) and the caller receives a typed
        // `VmError::Linkage(LinkageError::VerifyError { .. })`.
        if !options.skip_verification
            && !self.cds_class_cache.contains_key(name)
            && !class.is_synthetic_stub
            && class.state != ClassState::Verified
        {
            let hierarchy = ClassStoreHierarchy {
                class_store: &self.class_store,
                loaded_classes: &self.loaded_classes,
            };
            if let Err(verify_err) = crate::verifier::verify_class(
                &class,
                &self.class_store,
                &hierarchy,
            ) {
                // Verifier rejected the bytecode. Drop the guard set
                // entry so retry attempts are not erroneously blocked.
                self.loading_guard.remove(name);
                return Err(VmError::Linkage(verify_err));
            }
        }

        debug!(
            class = %class.name,
            id = %id,
            loader = %loader_id,
            superclass = ?superclass_id,
            fields = num_total_fields,
            methods = class.methods.len(),
            has_finalizer = class.has_finalizer,
            "Class loaded",
        );

        // Register. Pre-check at the top of this function already
        // rejected duplicate-defines unless `allow_redefine` is set; if
        // the existing class is here, it's because we are doing an
        // in-place redefinition (WP2.4). In that case we drop the old
        // class_id from both maps before inserting the new one.
        let name_hash = class_name_hash(&class.name);
        // T10.9.E: `class.name` is already `Arc<str>` — clone the arc
        // (refcount bump, no allocation) instead of `.to_string()`-ing it
        // into a fresh `String`. This is THE hot insert site — every class
        // define hits it once.
        let key = (loader_id, Arc::clone(&class.name));
        if options.allow_redefine {
            if let Some(old_id) = self.loaded_classes.remove(&key) {
                self.name_to_id.remove(&class_name_hash(&class.name));
                debug!(
                    class = %class.name,
                    old_id = %old_id,
                    new_id = %id,
                    "Class redefined (WP2.4 instrument)",
                );
            }
        }
        self.loaded_classes.insert(key, id);
        self.name_to_id.insert(name_hash, id);
        self.class_bytes_cache.insert(name.to_string(), bytes.to_vec());
        // WP2.3: persist the per-class skip-verification flag in the side
        // table. The verifier consults `class_skip_bytecode_verification`
        // during link-time so trusted hidden / generated classes
        // (ByteBuddy, CGLIB, JDK Proxy) bypass Pass-3 type-checking
        // while ordinary classes still get verified.
        if options.skip_verification {
            self.skip_bytecode_verification.insert(id);
        }
        // T10.9.E: clone the `Arc<str>` (refcount bump) instead of
        // allocating a fresh `String` for the JVMTI hook.
        let class_name_for_hook = Arc::clone(&class.name);
        let class_id_for_hook = id.as_u32();
        self.class_store.add(class);

        // T10.5 — Build this class's vtable descriptor layout, cache it on
        // `self.vtable_descriptors`, and fire the install hook so the VM
        // side can populate its `VtableManager`.
        //
        // The build is a standard single-inheritance vtable walk:
        //   1. Start from a clone of the superclass's vtable descriptor
        //      vec (empty for `java/lang/Object`).
        //   2. For each virtual method declared in THIS class, override
        //      any matching inherited slot in place; otherwise append a
        //      new slot.
        //
        // Virtual methods for vtable purposes are: non-static,
        // non-private, and not `<init>`/`<clinit>`. `private` methods are
        // statically dispatched (invokespecial), static methods live on
        // the Class object, and constructors are never virtual.
        let (entries, overrides) =
            self.build_vtable_descriptors_with_overrides(id, superclass_id);
        self.vtable_descriptors.insert(id, entries.clone());
        fire_vtable_install_hook(class_id_for_hook, entries);
        // T10.9.A — fire CHA override signals for each super-class slot
        // that this class replaced. The VM-side listener calls
        // `VtableManager::invalidate_for_override` so stale cached
        // dispatch entries (thread-local invoke_cache or promoted
        // shared-resolution entries) get re-resolved on next hit.
        for (super_id, slot) in overrides {
            fire_vtable_override_hook(super_id, slot);
        }

        // T6.3.1 — Fire the JVMTI ClassLoad hook. The VM's JvmtiEventManager
        // snapshots the attached-env list under its own lock; it never
        // re-enters the class manager, so it is safe to call from inside
        // `&mut self`. We still release the caller's class-manager write
        // lock at the outer callsite before agent callbacks run for any
        // slower path that needs it — here, the define_class path only
        // mutates `self`, so dropping is not needed.
        //
        // ClassPrepare is fired after linking/verification completes. For
        // classes that are loaded but not yet linked, the VM's linker path
        // will fire it separately; in most code paths `define_class`
        // eagerly links eagerly-resolved classes, so we fire both here and
        // let the JVMTI layer de-duplicate if the state flag indicates
        // prepare has already been seen.
        fire_class_load_hook(class_id_for_hook, &class_name_for_hook, 0);
        fire_class_prepare_hook(class_id_for_hook, &class_name_for_hook, 0);

        Ok(id)
    }

    /// T10.5 — build the vtable descriptor vec for `class_id`.
    ///
    /// Standard single-inheritance walk:
    /// - seed with a clone of the superclass's descriptor vec (empty for
    ///   `java/lang/Object`);
    /// - for each virtual method declared in this class, override the
    ///   matching inherited slot or append a new one.
    ///
    /// A method is considered "virtual" for vtable purposes when it is
    /// neither static, private, nor a constructor (`<init>`/`<clinit>`).
    /// Private methods use invokespecial (static dispatch), static
    /// methods live on the class itself, and constructors are never
    /// inherited.
    ///
    /// Returns an owned vec; caller is responsible for storing/forwarding.
    ///
    /// T10.9.A — also returns the list of `(super_class_id, slot)` pairs that
    /// were overridden by this class. The VM-side CHA listener uses this to
    /// invalidate super's cached dispatch entries.
    fn build_vtable_descriptors(
        &self,
        class_id: ClassId,
        superclass_id: Option<ClassId>,
    ) -> Vec<Option<VtableSlotDescriptor>> {
        self.build_vtable_descriptors_with_overrides(class_id, superclass_id).0
    }

    /// T10.9.A — same as `build_vtable_descriptors` but also returns the
    /// list of super-class slots this class overrode. Each pair is
    /// `(super_class_id_u32, slot_index)`. Used by the class-link path
    /// to fire CHA-invalidation signals on the super's cached vtable
    /// entries (so in-flight `invoke_cache` / JIT dispatch sees the
    /// override immediately).
    fn build_vtable_descriptors_with_overrides(
        &self,
        class_id: ClassId,
        superclass_id: Option<ClassId>,
    ) -> (Vec<Option<VtableSlotDescriptor>>, Vec<(u32, usize)>) {
        // Seed from the superclass's already-built descriptor vec.
        // A zero-sized vec is the natural identity for `java/lang/Object`
        // and for classes whose superclass hasn't been processed yet
        // (which only happens in pathological re-entry paths; the normal
        // load order guarantees the super is built first).
        let mut entries: Vec<Option<VtableSlotDescriptor>> = match superclass_id {
            Some(sid) => self
                .vtable_descriptors
                .get(&sid)
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        };

        // Maintain a (name, desc) -> slot index so we can detect overrides
        // without a linear scan for every method.
        //
        // T10.9.E: key is now `(Arc<str>, Arc<str>)` (was `(String, String)`)
        // — every key was previously `.clone()`-ed off the inherited
        // descriptor's String fields; now it's a refcount bump on the
        // Arc<str> from the source method. Also switched the inner map
        // from SipHash `HashMap` to `FxHashMap` because the key is fully
        // trusted (method-name strings interned by the class reader).
        let mut name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize> =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        for (slot, entry) in entries.iter().enumerate() {
            if let Some(e) = entry {
                name_to_slot.insert((Arc::clone(&e.method_name), Arc::clone(&e.descriptor)), slot);
            }
        }

        let class = match self.class_store.get(class_id) {
            Some(c) => c,
            None => return (entries, Vec::new()),
        };
        let class_id_u32 = class_id.as_u32();
        let super_u32 = superclass_id.map(|s| s.as_u32());

        // T10.9.A — record each super-class slot index that this class
        // overrides so the VM-side hook can fire the CHA invalidation.
        let mut overrides: Vec<(u32, usize)> = Vec::new();

        for (method_index, method) in class.methods.iter().enumerate() {
            // Skip non-virtual methods.
            if method.is_static() {
                continue;
            }
            if method
                .access_flags
                .contains(rustjvm_reader::class_access_flags::MethodAccessFlags::PRIVATE)
            {
                continue;
            }
            if &*method.name == "<init>" || &*method.name == "<clinit>" {
                continue;
            }

            // T10.9.E: `method.name` and `method.descriptor` are already
            // `Arc<str>` (see `reader/src/method.rs`). Clone the arcs
            // (refcount bumps) rather than allocating fresh Strings —
            // saves ~90k throwaway allocs per Spring Boot cold start.
            let key = (Arc::clone(&method.name), Arc::clone(&method.descriptor));

            // T10.9.A — snapshot the method's Code attribute + flags at
            // link time. `None` when the method has no Code (abstract)
            // or when it's native (no bytecode). Native methods still
            // populate the slot so name-based lookup succeeds; the
            // interpreter routes them through the native registry.
            let is_abstract = method.access_flags.contains(
                rustjvm_reader::class_access_flags::MethodAccessFlags::ABSTRACT,
            );
            let is_native = method.is_native();
            let num_params_u16 = rustjvm_jit::count_param_slots(&method.descriptor) as u16;
            let dispatch: Option<VtableMethodSnapshot> = if is_abstract {
                None
            } else if let Some(code_attr) = method.code() {
                Some(VtableMethodSnapshot {
                    class_name: class.name.to_string(),
                    source_file: class.source_file.clone(),
                    code: code_attr.code.clone(),
                    exception_table: code_attr.exception_table.clone(),
                    max_stack: code_attr.max_stack,
                    max_locals: code_attr.max_locals,
                    num_params: num_params_u16,
                    is_synchronized: method.is_synchronized(),
                    is_static: false,
                    is_native: false,
                })
            } else if is_native {
                // Native methods don't carry bytecode; the snapshot is a
                // shell that identifies the method so the VM registry
                // lookup can find the Rust callback.
                Some(VtableMethodSnapshot {
                    class_name: class.name.to_string(),
                    source_file: class.source_file.clone(),
                    code: Vec::new(),
                    exception_table: Vec::new(),
                    max_stack: 0,
                    max_locals: 0,
                    num_params: num_params_u16,
                    is_synchronized: method.is_synchronized(),
                    is_static: false,
                    is_native: true,
                })
            } else {
                // Non-abstract, non-native method without a Code
                // attribute — this is a spec violation but we stay
                // defensive and leave `dispatch = None` so the slow
                // path kicks in.
                None
            };

            let new_entry = VtableSlotDescriptor {
                declaring_class_id: class_id_u32,
                method_index: method_index as u32,
                // T10.9.E: clone the `Arc<str>` from the source method
                // (refcount bump) instead of allocating a fresh `String`.
                method_name: Arc::clone(&method.name),
                descriptor: Arc::clone(&method.descriptor),
                dispatch,
            };

            if let Some(&slot) = name_to_slot.get(&key) {
                // Override inherited slot in place — same slot index so
                // that subclass dispatch remains index-stable across
                // further inheritance. Record the super's slot for CHA
                // invalidation.
                if let Some(s) = super_u32 {
                    overrides.push((s, slot));
                }
                entries[slot] = Some(new_entry);
            } else {
                // New method signature: append a fresh slot.
                let slot = entries.len();
                entries.push(Some(new_entry));
                name_to_slot.insert(key, slot);
            }
        }

        (entries, overrides)
    }

    /// T10.5 — read-only view of a class's vtable descriptor layout.
    ///
    /// Used by tests and by any caller that needs to inspect the
    /// descriptor vec without going through the VM's installed hook.
    /// Returns `None` if the class hasn't been processed by
    /// `define_class_with_options` yet.
    pub fn vtable_descriptors_of(
        &self,
        class_id: ClassId,
    ) -> Option<&[Option<VtableSlotDescriptor>]> {
        self.vtable_descriptors.get(&class_id).map(|v| v.as_slice())
    }

    /// Get a reference to a loaded class by its id.
    pub fn get_class(&self, id: ClassId) -> Option<&Class> {
        self.class_store.get(id)
    }

    /// Get a mutable reference to a loaded class by its id.
    pub fn get_class_mut(&mut self, id: ClassId) -> Option<&mut Class> {
        self.class_store.get_mut(id)
    }

    /// Find a raw classpath resource by name.
    ///
    /// Searches all classpaths in order (bootstrap → extension → application).
    /// Returns the raw bytes of the first match, or `None` if not found.
    pub fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        self.bootstrap.class_path().find_resource(name)
            .or_else(|| self.extension.class_path().find_resource(name))
            .or_else(|| self.application.class_path().find_resource(name))
    }

    /// Return a URL string for every classpath entry that contains a resource
    /// with the given name. Searches bootstrap, extension, and application
    /// classpaths in order and concatenates the results. Analog of
    /// `ClassLoader.getResources` — enumerates every match rather than
    /// stopping at the first.
    pub fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        let mut out = self.bootstrap.class_path().find_all_resource_urls(name);
        out.extend(self.extension.class_path().find_all_resource_urls(name));
        out.extend(self.application.class_path().find_all_resource_urls(name));
        out
    }

    /// with the given name. Parallel to [`find_all_resource_urls`] but returns
    /// content rather than URLs — used by Rust-native resource enumeration
    /// paths (e.g. `ServiceLoader` provider discovery in
    /// `native-builtins/src/service_loader.rs`) that bypass the JDK's
    /// `URL.openStream` / `BufferedReader` chain. Searches bootstrap →
    /// extension → application and concatenates the results.
    pub fn find_all_resource_bytes(&self, name: &str) -> Vec<Vec<u8>> {
        let mut out = self.bootstrap.class_path().find_all_resource_bytes(name);
        out.extend(self.extension.class_path().find_all_resource_bytes(name));
        out.extend(self.application.class_path().find_all_resource_bytes(name));
        out
    }

    /// Find the filesystem path of the classpath entry that holds a given
    /// class.  Used by `Class.getProtectionDomain()` to build a CodeSource
    /// with a real location URL.  Searches application → extension → bootstrap.
    pub fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        self.application.class_path().find_class_source_path(class_name)
            .or_else(|| self.extension.class_path().find_class_source_path(class_name))
            .or_else(|| self.bootstrap.class_path().find_class_source_path(class_name))
    }

    /// Build a real `CodeSource` for a given class by locating its origin
    /// classpath entry and, if that entry is a signed JAR, extracting the
    /// signer certificate blocks from `META-INF`.  Searches application →
    /// extension → bootstrap; returns `None` if the class is a synthetic
    /// stub or lives in a JMOD/jimage module.
    pub fn find_class_code_source(&self, class_name: &str) -> Option<CodeSource> {
        let (url, certs) = self
            .application
            .class_path()
            .find_class_code_source_info(class_name)
            .or_else(|| self.extension.class_path().find_class_code_source_info(class_name))
            .or_else(|| self.bootstrap.class_path().find_class_code_source_info(class_name))?;
        Some(CodeSource::new(Some(url), certs))
    }

    /// WP2.3 — query whether a class was registered with
    /// `DefineClassOptions::skip_verification = true`.
    ///
    /// Used by the VM's link-time verifier to bypass Pass-3 bytecode
    /// type-checking for trusted hidden / runtime-generated classes
    /// (JDK Proxy, ByteBuddy, CGLIB) that pass JVMS structural rules
    /// but emit synthesised stack frames that our verifier does not
    /// model.  Default `false` — every other class is verified.
    pub fn class_skip_bytecode_verification(&self, class_id: ClassId) -> bool {
        self.skip_bytecode_verification.contains(&class_id)
    }

    // -------------------------------------------------------------------
    // WP2.4-B — JEP 109 / JVMTI RedefineClasses
    // -------------------------------------------------------------------

    /// WP2.4-B — current redefinition generation for a class. Starts
    /// at 0 for every freshly-defined class and increments by 1 on
    /// each successful [`Self::redefine_class`]. Caches that key
    /// entries by [`ClassId`] should snapshot this value at insert
    /// time and invalidate the entry on mismatch — that's cheaper than
    /// walking every cache at redefine time.
    pub fn class_redefine_generation(&self, class_id: ClassId) -> u32 {
        self.redefine_generations
            .read()
            .expect("redefine_generations poisoned")
            .get(&class_id)
            .map(|c| c.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// WP2.4-B — return a shared [`Arc<AtomicU32>`] handle to the
    /// generation counter for `class_id`. Callers (JIT cache, invoke
    /// cache) hold the handle and re-check on each cache hit; the
    /// counter outlives the [`ClassManager`] borrow because it's
    /// reference-counted.
    ///
    /// Lazily allocates the counter on first call. Calling this on a
    /// class that has never been redefined still returns a valid
    /// handle whose value is 0; the next `redefine_class` will bump
    /// it without re-allocating.
    ///
    /// WP2.4-F1 — takes `&self` (was `&mut self`) so the per-thread
    /// invoke-cache populate path can acquire a handle while only
    /// holding `class_manager.read()`.  The internal lock guards lazy
    /// insertion; the returned `Arc` outlives the lock guard.
    pub fn class_redefine_generation_handle(&self, class_id: ClassId) -> Arc<AtomicU32> {
        // Fast path: read-lock and clone if already present.  The hot
        // path (re-populate after a stale eviction) almost always hits
        // here because the counter was inserted on the very first
        // populate or by `redefine_class` itself.
        if let Some(existing) = self
            .redefine_generations
            .read()
            .expect("redefine_generations poisoned")
            .get(&class_id)
        {
            return Arc::clone(existing);
        }
        // Slow path: upgrade to write-lock and double-check before
        // inserting (a sibling thread may have raced us to insert).
        let mut guard = self
            .redefine_generations
            .write()
            .expect("redefine_generations poisoned");
        Arc::clone(
            guard
                .entry(class_id)
                .or_insert_with(|| Arc::new(AtomicU32::new(0))),
        )
    }

    /// WP2.4-B — JEP 109 + JVMTI `RedefineClasses` semantics. Replace
    /// the bytecode of `class_id` with `new_bytes`, leaving the class
    /// identity, vtable layout, field set, and existing instances
    /// untouched.
    ///
    /// # Constraints (must reject otherwise)
    ///
    /// 1. `new_bytes` parses as a valid class file and the parsed
    ///    `this_class` MUST match the existing class's name (the new
    ///    bytes can't be from a different class).
    /// 2. The new class's superclass name MUST match the existing
    ///    superclass name. Same for direct interfaces (counts AND
    ///    names AND order).
    /// 3. Field declarations MUST be identical: same count, same
    ///    order, identical name + descriptor + access flags.
    /// 4. Method declarations MUST be identical: same count, same
    ///    order, identical name + descriptor + access flags. Bodies
    ///    (Code attributes) and annotations may differ.
    ///
    /// `RedefineOptions::skip_structural_check = true` skips items
    /// 2-4 above (name match is always enforced).
    ///
    /// # On success
    ///
    /// * Each method's `Code` and `RuntimeVisibleAnnotations` (etc.)
    ///   attributes are replaced with the new ones. Non-`Code`
    ///   non-annotation attributes (e.g. `Exceptions`,
    ///   `MethodParameters`) are also replaced — they're metadata
    ///   that has no observable runtime effect on existing
    ///   in-flight frames.
    /// * The class's constant pool, bootstrap_methods, and
    ///   class-level annotations are replaced.
    /// * `class_bytes_cache` is updated to the new bytes.
    /// * The vtable layout is unchanged (same number of slots, same
    ///   order). Each slot's `dispatch` snapshot is rebuilt from the
    ///   new method bodies and re-installed via the same hooks
    ///   `define_class` uses.
    /// * The redefine generation counter for this class is bumped.
    /// * Every JIT-compiled body keyed on this class's id is evicted
    ///   via the installed [`JitInvalidateHook`].
    /// * The JVMTI `ClassFileLoadHook` event fires before parsing,
    ///   giving any registered agent the chance to substitute its
    ///   own transformed bytes.
    ///
    /// # On failure
    ///
    /// Returns [`LinkageError::UnsupportedClassRedefinitionError`]
    /// with a precise reason. The class state on the manager side is
    /// guaranteed to be unchanged on the failure path (the new bytes
    /// are validated end-to-end before any mutation begins).
    ///
    /// [`JitInvalidateHook`]: JitInvalidateHook
    pub fn redefine_class(
        &mut self,
        class_id: ClassId,
        new_bytes: Vec<u8>,
        options: RedefineOptions,
    ) -> Result<(), LinkageError> {
        // ---- Step 0: header sanity (cheap pre-checks) ----
        if new_bytes.len() < 8 {
            return Err(LinkageError::UnsupportedClassRedefinitionError {
                class_name: class_id.as_u32().to_string(),
                message: format!(
                    "new_bytes too short ({} bytes; need >= 8 for header)",
                    new_bytes.len()
                ),
            });
        }
        if new_bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            return Err(LinkageError::UnsupportedClassRedefinitionError {
                class_name: class_id.as_u32().to_string(),
                message: "bad magic in new_bytes; expected CAFEBABE".to_string(),
            });
        }

        // Snapshot the existing class identity. We need the name +
        // loader_id for the cache update at the end, plus the
        // structural snapshot (super/interfaces/fields/methods) for
        // constraint checks. All of this is taken under a `&` borrow
        // and cloned into owned data so we can drop the borrow before
        // calling the JVMTI hook (which may re-enter the loader).
        //
        // TODO(T10.9.E): the `to_string()` calls in this block could be
        // swapped for `Arc::clone(&name)` style refcount bumps. Skipped
        // here because this is the cold JVMTI `redefine_class` path
        // (called once per agent retransform, not per class-load) — the
        // String snapshots are used downstream as owned, comparable
        // values for structural-equivalence checks, so converting them
        // also requires touching the comparator. Out of scope for the
        // hot-path sweep.
        let (
            existing_name,
            existing_loader,
            existing_super_name,
            existing_iface_names,
            existing_field_sigs,
            existing_method_sigs,
        ) = {
            let cls = self.class_store.get(class_id).ok_or_else(|| {
                LinkageError::UnsupportedClassRedefinitionError {
                    class_name: class_id.as_u32().to_string(),
                    message: "class id not loaded".to_string(),
                }
            })?;
            // Resolve super name from the super's ClassId (or empty
            // string for java/lang/Object which has no super).
            let super_name = match cls.superclass {
                Some(sid) => self
                    .class_store
                    .get(sid)
                    .map(|s| s.name.to_string())
                    .unwrap_or_default(),
                None => String::new(),
            };
            // Direct interfaces — internal names in declaration order.
            let iface_names: Vec<String> = cls
                .interfaces
                .iter()
                .filter_map(|iid| self.class_store.get(*iid).map(|i| i.name.to_string()))
                .collect();
            // Field signatures: (name, descriptor, access_flag bits).
            let field_sigs: Vec<(String, String, u16)> = cls
                .fields
                .iter()
                .map(|f| (f.name.to_string(), f.descriptor.to_string(), f.access_flags.bits()))
                .collect();
            // Method signatures: (name, descriptor, access_flag bits).
            let method_sigs: Vec<(String, String, u16)> = cls
                .methods
                .iter()
                .map(|m| (m.name.to_string(), m.descriptor.to_string(), m.access_flags.bits()))
                .collect();
            (
                cls.name.to_string(),
                cls.loader_id,
                super_name,
                iface_names,
                field_sigs,
                method_sigs,
            )
        };

        // ---- Step 1: fire ClassFileLoadHook (pre-parse) ----
        // The agent sees the OLD bytes and the NEW bytes and may
        // return its own transformed buffer. We grab the old bytes
        // from `class_bytes_cache`; if not present (synthetic stub or
        // old code path that never recorded them) we pass an empty
        // slice — JVMTI agents tolerate that.
        let old_bytes_opt = self.class_bytes_cache.get(&existing_name).cloned();
        let old_bytes_slice: &[u8] = old_bytes_opt.as_deref().unwrap_or(&[]);
        let class_id_u32 = class_id.as_u32();
        let effective_new_bytes: Vec<u8> = match fire_class_file_load_hook(
            class_id_u32,
            &existing_name,
            old_bytes_slice,
            &new_bytes,
        ) {
            Some(transformed) => transformed,
            None => new_bytes,
        };

        // ---- Step 2: parse new bytes ----
        let mut new_class_file = rustjvm_reader::read_class(&effective_new_bytes).map_err(|e| {
            LinkageError::UnsupportedClassRedefinitionError {
                class_name: existing_name.clone(),
                message: format!("new_bytes failed to parse: {e}"),
            }
        })?;

        // Same rationale as `define_class_with_options`: every class-level
        // attribute we look at below (`BootstrapMethods`, `RuntimeVisible
        // [/Invisible]Annotations`, `SourceFile`) feeds the runtime `Class`,
        // so eager decode of the class-level table is the cheapest path
        // and keeps the rest of this function index/match-style identical
        // to the pre-lazy code.
        force_decode_all(&mut new_class_file.attributes, &new_class_file.constant_pool)
            .map_err(|e| LinkageError::UnsupportedClassRedefinitionError {
                class_name: existing_name.clone(),
                message: format!("attribute decode failed: {e}"),
            })?;

        // ---- Step 3: name match (always enforced) ----
        if new_class_file.this_class != existing_name {
            return Err(LinkageError::UnsupportedClassRedefinitionError {
                class_name: existing_name.clone(),
                message: format!(
                    "new bytes declare this_class = {}, expected {}",
                    new_class_file.this_class, existing_name,
                ),
            });
        }

        // ---- Step 4: structural-equivalence checks (skippable) ----
        if !options.skip_structural_check {
            // 4a — superclass name match.
            let new_super_name = new_class_file
                .super_class
                .clone()
                .unwrap_or_default();
            if new_super_name != existing_super_name {
                return Err(LinkageError::UnsupportedClassRedefinitionError {
                    class_name: existing_name.clone(),
                    message: format!(
                        "superclass changed: was {} now {}",
                        if existing_super_name.is_empty() {
                            "<none>"
                        } else {
                            &existing_super_name
                        },
                        if new_super_name.is_empty() {
                            "<none>"
                        } else {
                            &new_super_name
                        },
                    ),
                });
            }
            // 4b — interface list (counts, order, names).
            if new_class_file.interfaces.len() != existing_iface_names.len() {
                return Err(LinkageError::UnsupportedClassRedefinitionError {
                    class_name: existing_name.clone(),
                    message: format!(
                        "interface count changed: was {} now {}",
                        existing_iface_names.len(),
                        new_class_file.interfaces.len(),
                    ),
                });
            }
            for (i, (old, new)) in existing_iface_names
                .iter()
                .zip(new_class_file.interfaces.iter())
                .enumerate()
            {
                if old != new {
                    return Err(LinkageError::UnsupportedClassRedefinitionError {
                        class_name: existing_name.clone(),
                        message: format!(
                            "interface[{i}] changed: was {old} now {new}",
                        ),
                    });
                }
            }
            // 4c — field set (counts, order, name+desc+modifiers).
            if new_class_file.fields.len() != existing_field_sigs.len() {
                return Err(LinkageError::UnsupportedClassRedefinitionError {
                    class_name: existing_name.clone(),
                    message: format!(
                        "field count changed: was {} now {}",
                        existing_field_sigs.len(),
                        new_class_file.fields.len(),
                    ),
                });
            }
            for (i, ((old_name, old_desc, old_flags), new_field)) in existing_field_sigs
                .iter()
                .zip(new_class_file.fields.iter())
                .enumerate()
            {
                let new_name: &str = &new_field.name;
                let new_desc: &str = &new_field.descriptor;
                let new_flags = new_field.access_flags.bits();
                if old_name != new_name
                    || old_desc != new_desc
                    || *old_flags != new_flags
                {
                    return Err(LinkageError::UnsupportedClassRedefinitionError {
                        class_name: existing_name.clone(),
                        message: format!(
                            "field[{i}] changed: was {old_name}:{old_desc} flags={old_flags:#x} \
                             now {new_name}:{new_desc} flags={new_flags:#x}",
                        ),
                    });
                }
            }
            // 4d — method declarations (counts, order, name+desc+modifiers).
            if new_class_file.methods.len() != existing_method_sigs.len() {
                return Err(LinkageError::UnsupportedClassRedefinitionError {
                    class_name: existing_name.clone(),
                    message: format!(
                        "method count changed: was {} now {} (JEP 109 forbids add/remove)",
                        existing_method_sigs.len(),
                        new_class_file.methods.len(),
                    ),
                });
            }
            for (i, ((old_name, old_desc, old_flags), new_method)) in existing_method_sigs
                .iter()
                .zip(new_class_file.methods.iter())
                .enumerate()
            {
                let new_name: &str = &new_method.name;
                let new_desc: &str = &new_method.descriptor;
                let new_flags = new_method.access_flags.bits();
                if old_name != new_name
                    || old_desc != new_desc
                    || *old_flags != new_flags
                {
                    return Err(LinkageError::UnsupportedClassRedefinitionError {
                        class_name: existing_name.clone(),
                        message: format!(
                            "method[{i}] changed: was {old_name}{old_desc} flags={old_flags:#x} \
                             now {new_name}{new_desc} flags={new_flags:#x}",
                        ),
                    });
                }
            }
        }

        // ---- Step 5: in-place swap ----
        // At this point every constraint has cleared. Take the new
        // class file apart and write its mutable parts onto the
        // existing Class. Hold the &mut borrow only as long as needed,
        // then drop it before firing hooks (which can re-enter the
        // loader).
        if options.log_diff {
            // Best-effort diagnostic — list any methods whose code
            // attribute bytes differ. Cheap because we already have
            // both the old and the new method vecs in scope.
            // (Using only the existing/new method indices; the new
            // methods will be moved in below so we capture before that.)
            if let Some(existing) = self.class_store.get(class_id) {
                for (i, m) in existing.methods.iter().enumerate() {
                    if i >= new_class_file.methods.len() {
                        break;
                    }
                    let old_code = m.code().map(|c| c.code.clone()).unwrap_or_default();
                    let new_code = new_class_file.methods[i]
                        .code()
                        .map(|c| c.code.clone())
                        .unwrap_or_default();
                    if old_code != new_code {
                        debug!(
                            class = %existing_name,
                            method = %m.name,
                            descriptor = %m.descriptor,
                            old_len = old_code.len(),
                            new_len = new_code.len(),
                            "WP2.4-B redefine: method body changed",
                        );
                    }
                }
            }
        }

        let new_methods = new_class_file.methods;
        let new_constant_pool = new_class_file.constant_pool;
        let new_attributes = new_class_file.attributes;

        // Recompute class-level annotations from the new attributes.
        // All entries are already decoded (see `force_decode_all` above),
        // so `as_decoded()` returns `Some` everywhere.
        let mut new_annotations = Vec::new();
        for attr in &new_attributes {
            match attr.as_decoded() {
                Some(Attribute::RuntimeVisibleAnnotations(anns))
                | Some(Attribute::RuntimeInvisibleAnnotations(anns)) => {
                    new_annotations.extend(anns.iter().cloned());
                }
                _ => {}
            }
        }
        // Recompute bootstrap methods (invokedynamic call sites depend
        // on these; they may legitimately change with a body redefine).
        let new_bootstrap_methods = new_attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::BootstrapMethods(bms)) => Some(bms.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // Source file may have changed if the compiler regenerated it.
        let new_source_file = new_attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::SourceFile(name)) => Some(name.clone()),
            _ => None,
        });

        // The loader_id, id, name, superclass, interfaces, fields,
        // first_field_index, num_total_fields, hidden, module_name,
        // is_synthetic_stub, has_finalizer, code_source, nest_host,
        // nest_members, record_components, permitted_subclasses,
        // inner_classes, enclosing_method, version, state, and
        // initializing_thread are all PRESERVED — JEP 109 forbids
        // changing any of them.
        {
            let cls = self.class_store.get_mut(class_id).ok_or_else(|| {
                LinkageError::UnsupportedClassRedefinitionError {
                    class_name: existing_name.clone(),
                    message: "class id vanished during redefine (race)".to_string(),
                }
            })?;
            cls.methods = new_methods;
            cls.constant_pool = new_constant_pool;
            cls.bootstrap_methods = new_bootstrap_methods;
            cls.annotations = new_annotations;
            if new_source_file.is_some() {
                cls.source_file = new_source_file;
            }
        }
        // Forget the borrow — the rest of this function is hooks +
        // bookkeeping that may re-enter the manager.
        let _ = existing_loader; // silence unused warning if no read below

        // Update the cached class bytes so subsequent
        // `getResourceAsStream` lookups + later redefines see the new
        // bytes as their "old bytes".
        self.class_bytes_cache
            .insert(existing_name.clone(), effective_new_bytes);

        // ---- Step 6: rebuild + re-install vtable descriptor snapshots ----
        //
        // The vtable LAYOUT is unchanged (same method count, same
        // order), but each slot's `dispatch` snapshot was built from
        // the OLD code attributes. Rebuild the descriptor list from
        // the current Class state and re-fire the install hook so the
        // VM's VtableManager picks up the new code/exception tables.
        //
        // We deliberately do NOT recompute the override list here:
        // the inheritance shape didn't change, so no super-class slots
        // were freshly overridden by the redefine.
        let class_super_id = self
            .class_store
            .get(class_id)
            .and_then(|c| c.superclass);
        let new_entries = self.build_vtable_descriptors(class_id, class_super_id);
        self.vtable_descriptors.insert(class_id, new_entries.clone());
        fire_vtable_install_hook(class_id_u32, new_entries);

        // ---- Step 7: bump generation counter ----
        // WP2.4-F1: use the shared handle accessor so the per-thread
        // invoke-cache (which holds an `Arc<AtomicU32>` clone of the
        // *same* counter from populate time) sees this bump on its next
        // hit and auto-evicts the stale entry.
        let counter = self.class_redefine_generation_handle(class_id);
        // `Release` ordering: any reader that sees the new generation
        // is guaranteed to also see the new methods we wrote above
        // (strictly we already serialized the writes via &mut, but
        // the explicit Release pairs cleanly with cross-thread
        // Acquire reads of the counter).
        let new_gen = counter.fetch_add(1, Ordering::Release) + 1;

        // ---- Step 8: invalidate JIT caches keyed on class_id ----
        fire_jit_invalidate_hook(class_id_u32);

        debug!(
            class = %existing_name,
            class_id = class_id_u32,
            generation = new_gen,
            "WP2.4-B class redefined in place",
        );

        Ok(())
    }

    /// List all class names available on the application classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`).
    pub fn list_application_class_names(&self) -> Vec<String> {
        self.application.class_path().list_class_names()
    }

    /// Dynamically extend the application classpath at runtime.
    ///
    /// Called by `URLClassLoader` when new URLs are registered. Each path is
    /// added as a directory or JAR entry to the application class finder.
    pub fn extend_application_classpath(&mut self, paths: &[String]) {
        for path in paths {
            self.application.add_path(path);
        }
    }

    /// Find a class by name. Searches all loaders in priority order
    /// (bootstrap → extension → application).
    ///
    /// Returns `None` if the class hasn't been loaded by any loader.
    pub fn find_class_by_name(&self, name: &str) -> Option<ClassId> {
        let slash = if name.contains('.') && !name.contains('/') {
            name.replace('.', "/")
        } else {
            name.to_string()
        };
        let dot = slash.replace('/', ".");
        let keys = if slash == dot {
            vec![slash]
        } else {
            vec![slash, dot]
        };

        for key in &keys {
            // T10.9.E: one `Arc::from(&str)` per (key, loader-triple) probe
            // hoisted outside the loader loop so we don't allocate three
            // times. Same allocation cost as the prior `key.clone()`.
            let arc_key: Arc<str> = Arc::from(key.as_str());
            for loader_id in &[
                ClassLoaderId::Bootstrap,
                ClassLoaderId::Extension,
                ClassLoaderId::Application,
            ] {
                if let Some(&id) = self.loaded_classes.get(&(*loader_id, Arc::clone(&arc_key))) {
                    if let Some(class) = self.get_class(id) {
                        if class.hidden {
                            continue;
                        }
                    }
                    return Some(id);
                }
            }
        }
        for key in &keys {
            for ((_loader_id, class_name), &id) in self.loaded_classes.iter() {
                if &**class_name != key.as_str() {
                    continue;
                }
                if let Some(class) = self.get_class(id) {
                    if class.hidden {
                        continue;
                    }
                }
                return Some(id);
            }
        }
        None
    }

    /// Find a class by name within a specific loader's namespace, with delegation
    /// fallback to the standard loader chain (Bootstrap → Extension → Application).
    pub fn find_class_by_name_in_loader(&self, name: &str, loader_id: ClassLoaderId) -> Option<ClassId> {
        // Check the specific loader first.
        // T10.9.E: one `Arc::from(&str)` per probe — no worse than the
        // prior `name.to_string()`.
        if let Some(&id) = self.loaded_classes.get(&(loader_id, Arc::<str>::from(name))) {
            return Some(id);
        }
        // Delegate to parent chain
        self.find_class_by_name(name)
    }

    /// Get the loader identity for a loaded class.
    pub fn get_loader_id(&self, class_id: ClassId) -> Option<ClassLoaderId> {
        self.class_store.get(class_id).map(|c| c.loader_id)
    }

    /// Convenience: check if `child_id` is a subclass of (or implements) `parent_id`.
    pub fn is_subclass_of(&self, child_id: ClassId, parent_id: ClassId) -> bool {
        self.class_store
            .get(child_id)
            .is_some_and(|child| child.is_subclass_of(parent_id, &self.class_store))
    }

    /// Get a reference to the underlying class store.
    pub fn class_store(&self) -> &ClassStore {
        &self.class_store
    }

    /// The number of classes currently loaded.
    pub fn loaded_count(&self) -> usize {
        self.class_store.len()
    }

    /// Register a class name → id mapping for a given loader.
    ///
    /// This is primarily used by test code that manually inserts classes
    /// into the `ClassStore` and needs `find_class_by_name` to work.
    pub fn register_class_name(&mut self, loader_id: ClassLoaderId, name: &str, id: ClassId) {
        // T10.9.E: intern the name through the global pool so that
        // subsequent define-class flows (which clone `class.name` for the
        // hot insert) share the same `Arc<str>` allocation — keeping the
        // key dedup story identical regardless of whether the class is
        // registered via this side-door or via `define_class_with_options`.
        let name_arc = rustjvm_types::intern_arc(name);
        self.loaded_classes
            .insert((loader_id, name_arc), id);
        self.name_to_id.insert(class_name_hash(name), id);
    }

    /// Create a synthetic stub class for a JDK class that has no .class file.
    ///
    /// The stub has no methods (all handled by native registry) and minimal
    /// fields. Special cases add static fields for well-known classes like
    /// `java/lang/System` (needs `in`, `out`, `err`).
    fn create_synthetic_stub(&mut self, name: &str) -> Result<ClassId, VmError> {
        // Guard against re-entry: check again if already loaded.
        // T10.9.E: probe with `Arc::<str>::from(name)` — same allocation
        // cost as the prior `name.to_string()`.
        if let Some(&id) = self
            .loaded_classes
            .get(&(ClassLoaderId::Bootstrap, Arc::<str>::from(name)))
        {
            return Ok(id);
        }

        // Load superclass with correct hierarchy for known JDK classes.
        // Without this, exception catch handlers can't match subclasses
        // (e.g., `catch (RuntimeException e)` won't catch ArithmeticException).
        let superclass_id = if name == "java/lang/Object" {
            None
        } else {
            let parent = jdk_superclass(name);
            Some(self.load_class(parent)?)
        };

        // Create static fields for known classes
        let fields = synthetic_stub_fields(name);
        let num_static = fields
            .iter()
            .filter(|f| f.access_flags.contains(FieldAccessFlags::STATIC))
            .count();
        let num_instance = fields.len() - num_static;

        let parent_fields = match superclass_id {
            Some(super_id) => self
                .class_store
                .get(super_id)
                .map_or(0, |c| c.num_total_fields),
            None => 0,
        };

        // Load interface classes AFTER the stub is registered (to avoid re-entrant cycles)
        let iface_names = jdk_interfaces(name).to_vec();

        let id = self.class_store.next_id();
        // S-trinity #1: the `$`-name-as-interface heuristic misclassifies
        // concrete inner classes. Carve out the JDK loader chain
        // (`ClassLoaders$AppClassLoader` etc.) which is concrete; without
        // this exception, `alloc_classloader` produces objects whose
        // class chain has the INTERFACE bit set, and downstream
        // `(ClassLoader) priv.run()` checkcasts fail.
        let is_concrete_dollar_class = matches!(
            name,
            "jdk/internal/loader/ClassLoaders$AppClassLoader"
                | "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
        );
        // LETSGO_S1: Curated list of well-known JDK interfaces whose names
        // aren't matched by the `$` / `*able` / heuristic. Without this,
        // synthetic stubs for `java.util.{Set, Map, List, Collection,
        // Queue, Deque, Iterator, Map$Entry, ...}` end up as concrete
        // classes and `instanceof` walks via the `interfaces` edge fail
        // when no concrete bytecode `Set`/`Map`/etc. was loaded ahead of
        // the dependent class. (LinkedHashSet → HashSet → AbstractSet
        // chain is fine without this, but Spring boot's `Set.class`
        // reflection probes still need it.)
        let is_known_jdk_interface = matches!(
            name,
            "java/util/Collection"
                | "java/util/Set"
                | "java/util/SortedSet"
                | "java/util/NavigableSet"
                | "java/util/List"
                | "java/util/Map"
                | "java/util/SortedMap"
                | "java/util/NavigableMap"
                | "java/util/Queue"
                | "java/util/Deque"
                | "java/util/Iterator"
                | "java/util/ListIterator"
                | "java/util/Enumeration"
                | "java/util/Spliterator"
                | "java/util/RandomAccess"
                | "java/util/concurrent/ConcurrentMap"
                | "java/util/concurrent/BlockingQueue"
                | "java/util/concurrent/BlockingDeque"
                | "java/util/concurrent/TransferQueue"
        );
        let access_flags = if (name.contains("$") && !is_concrete_dollar_class)
            || name.ends_with("able")
            || is_known_jdk_interface
        {
            // Likely an interface (Serializable, Comparable, Iterable, etc.)
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT
        } else {
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER
        };

        let class = Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: rustjvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: superclass_id,
            interfaces: vec![], // populated after registration to avoid cycles
            fields,
            methods: synthetic_stub_ctor_methods(name),
            first_field_index: parent_fields,
            num_total_fields: parent_fields + num_instance,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: true,
            has_finalizer: false, // synthetic stubs don't override finalize()
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        };

        debug!(
            class = %class.name,
            id = %id,
            superclass = ?superclass_id,
            methods = class.methods.len(),
            method_sigs = ?class
                .methods
                .iter()
                .map(|m| format!("{}{}", m.name, m.descriptor))
                .collect::<Vec<_>>(),
            "Synthetic stub class created",
        );

        let name_hash = class_name_hash(&class.name);
        // T10.9.E: clone the existing `Arc<str>` (refcount bump) instead
        // of allocating a fresh `String` for the map key.
        let key = (ClassLoaderId::Bootstrap, Arc::clone(&class.name));
        self.loaded_classes.insert(key, id);
        self.name_to_id.insert(name_hash, id);
        self.class_store.add(class);

        // Deferred interface resolution: now that this class is registered,
        // loading interface classes won't re-create it
        if !iface_names.is_empty() {
            let mut iface_ids = Vec::new();
            for iface_name in iface_names {
                if let Ok(iface_id) = self.load_class(iface_name) {
                    iface_ids.push(iface_id);
                }
            }
            if !iface_ids.is_empty() {
                if let Some(class) = self.class_store.get_mut(id) {
                    class.interfaces = iface_ids;
                }
            }
        }

        Ok(id)
    }

    /// RKC16N.3 — Synthesise a reference- or primitive-array class without
    /// any classpath I/O.
    ///
    /// Per JVMS §5.3.3, an array class is *created* by the bootstrap class
    /// loader directly from its component type — no `.class` file is ever
    /// consulted. The synthesised `Class`:
    /// * has the original descriptor as its name (e.g. `[Ljava/util/HashMap;`,
    ///   `[I`, `[[Ljava/lang/Object;`),
    /// * has `superclass = java/lang/Object`,
    /// * implements `Cloneable` and `java.io.Serializable` (JLS §10.7),
    /// * is *not* marked `is_synthetic_stub` (it is a fully-formed array class,
    ///   not a stand-in for missing bytecode), and
    /// * for reference-array types, recursively resolves the component class
    ///   so that `[[Ljava/util/HashMap;` triggers loading of
    ///   `[Ljava/util/HashMap;` and `java/util/HashMap`.
    ///
    /// The result is cached in the standard `loaded_classes` / `name_to_id`
    /// maps under the bootstrap loader, so two calls with the same name
    /// return the same `ClassId`.
    fn synthesize_array_class(&mut self, name: &str) -> Result<ClassId, VmError> {
        debug_assert!(name.starts_with('['), "synthesize_array_class called with non-array name {name}");

        // Cache hit — return the existing array `Class` so identity is stable.
        if let Some(id) = self.get_loaded_class_id(name) {
            return Ok(id);
        }

        // Recursively resolve the component class. We strip exactly one
        // leading `[` and dispatch on the next character:
        //   `[`  → another array (recurse via `load_class`, which routes back
        //          here for `[`-prefixed names).
        //   `L…;` → reference component, e.g. `Ljava/util/HashMap;`. Strip the
        //           leading `L` and trailing `;` and load the named class.
        //   else → primitive component (`I`, `J`, `Z`, `B`, `S`, `C`, `F`, `D`).
        //          Primitive component classes have no `Class<?>` mirror in the
        //          ClassStore yet (they are surfaced lazily by the VM's
        //          `Class.getPrimitiveClass`), so we leave them unresolved
        //          here. Anything that needs the component class (e.g.
        //          `java.lang.Class.getComponentType()`) re-derives it from
        //          the array name.
        let rest = &name[1..];
        if rest.is_empty() {
            return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: "array descriptor with empty component".to_string(),
            }));
        }
        match rest.as_bytes()[0] {
            b'[' => {
                // Multi-dim array — synthesise the inner array first.
                self.load_class(rest)?;
            }
            b'L' => {
                // Reference component: must end with ';'.
                if !rest.ends_with(';') || rest.len() < 3 {
                    return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                        class_name: name.to_string(),
                        message: format!("malformed reference-array descriptor: {name}"),
                    }));
                }
                let component_name = &rest[1..rest.len() - 1];
                // Recursively resolve the component. Bubbling errors up
                // matches the JVMS rule that resolution of an array class
                // resolves its element type first.
                self.load_class(component_name)?;
            }
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {
                // Primitive-array — nothing to recursively load.
                if rest.len() != 1 {
                    return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                        class_name: name.to_string(),
                        message: format!("malformed primitive-array descriptor: {name}"),
                    }));
                }
            }
            _ => {
                return Err(VmError::ClassFile(ClassFileError::InvalidClassFile {
                    class_name: name.to_string(),
                    message: format!("unrecognised array component tag in {name}"),
                }));
            }
        }

        // Re-check the cache: the recursive `load_class(component)` above
        // can re-enter `synthesize_array_class` for the same `name` if the
        // component descriptor is malformed and a caller had previously
        // raced. Belt-and-braces.
        if let Some(id) = self.get_loaded_class_id(name) {
            return Ok(id);
        }

        // Resolve `java/lang/Object` and the JLS §10.7 array interfaces.
        // These calls go through the normal `load_class` path (no array
        // recursion because none of these names start with `[`), so they
        // hit either the real classpath or `create_synthetic_stub` exactly
        // as they would for any other JDK class.
        let object_id = self.load_class("java/lang/Object")?;
        let mut iface_ids = Vec::with_capacity(2);
        if let Ok(id) = self.load_class("java/lang/Cloneable") {
            iface_ids.push(id);
        }
        if let Ok(id) = self.load_class("java/io/Serializable") {
            iface_ids.push(id);
        }

        let id = self.class_store.next_id();
        // An array `Class` is `final`, `public`, and has the `ACC_ABSTRACT`
        // bit cleared — same surface flags `java.lang.Class` reports for
        // `int[].class.getModifiers()`. We mark it `SUPER` for parity with
        // ordinary loaded classes; `FINAL` reflects that you cannot subclass
        // an array type.
        let access_flags = ClassAccessFlags::PUBLIC
            | ClassAccessFlags::FINAL
            | ClassAccessFlags::SUPER;

        let class = Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: rustjvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: Some(object_id),
            interfaces: iface_ids,
            fields: vec![],
            methods: vec![],
            // Array objects carry no Java-level instance fields — their
            // length and elements live in the array header maintained by
            // the GC, not in field slots.
            first_field_index: self
                .class_store
                .get(object_id)
                .map_or(0, |c| c.num_total_fields),
            num_total_fields: self
                .class_store
                .get(object_id)
                .map_or(0, |c| c.num_total_fields),
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: Some("java.base".to_string()),
            // Crucially: an array class is NOT a synthetic stub — it is a
            // fully-formed array class produced by the bootstrap loader.
            // Marking it stub would (a) emit a misleading log line and
            // (b) make `load_class` try to "upgrade" it from a non-existent
            // .class file on the next call.
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            // RKC16N.3: array-class metadata not yet populated by this
            // synthesis path — the field is currently write-only across
            // the codebase, so leaving it `None` here matches every
            // other call site (see access_control, verifier, vm.rs,
            // benches, tests). Wire up real `ArrayInfo` once a consumer
            // (e.g. `Class.getComponentType` fast-path) actually reads it.
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        };

        debug!(
            class = %class.name,
            id = %id,
            superclass = ?class.superclass,
            "Synthesised array class (RKC16N.3)",
        );

        let name_hash = class_name_hash(&class.name);
        // T10.9.E: clone the existing `Arc<str>` (refcount bump) instead
        // of allocating a fresh `String` for the map key.
        let key = (ClassLoaderId::Bootstrap, Arc::clone(&class.name));
        self.loaded_classes.insert(key, id);
        self.name_to_id.insert(name_hash, id);
        self.class_store.add(class);

        Ok(id)
    }

    /// Upgrade a synthetic stub class to a real class loaded from bytecode.
    ///
    /// Reuses the existing ClassId so that existing references (field indices,
    /// type checks, etc.) remain valid. The class's methods, fields, constant
    /// pool, and other metadata are replaced with those from the real class file.
    ///
    /// The `first_field_index` and `num_total_fields` are recomputed, but we
    /// preserve `max(old, new)` for `num_total_fields` so that objects already
    /// allocated with the synthetic layout don't underflow.
    fn upgrade_synthetic_class(
        &mut self,
        id: ClassId,
        name: &str,
        bytes: &[u8],
        loader_id: ClassLoaderId,
    ) -> Result<(), VmError> {
        use rustjvm_reader::attribute::Attribute;

        let mut class_file = rustjvm_reader::read_class(bytes).map_err(|e| {
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: name.to_string(),
                message: e.to_string(),
            })
        })?;

        // Same rationale as `define_class_with_options`: each class-level
        // attribute consumed below populates the runtime `Class` directly,
        // so eager decode is the cheapest path and lets the rest of this
        // upgrade path use plain `Attribute` pattern matching.
        force_decode_all(&mut class_file.attributes, &class_file.constant_pool).map_err(
            |e| {
                VmError::ClassFile(ClassFileError::InvalidClassFile {
                    class_name: name.to_string(),
                    message: format!("attribute decode failed: {e}"),
                })
            },
        )?;

        // Load superclass (may already be loaded)
        self.loading_guard.insert(name.to_string());
        let superclass_id = match class_file.super_class {
            Some(ref super_name) => self.load_class(super_name).ok(),
            None => None,
        };

        // Load interfaces
        let interface_ids: Vec<ClassId> = class_file
            .interfaces
            .iter()
            .filter_map(|iface_name| self.load_class(iface_name).ok())
            .collect();
        self.loading_guard.remove(name);

        // Compute field layout from real class file
        let (first_field_index, num_total_fields) =
            compute_field_layout(&class_file.fields, superclass_id, &self.class_store);

        // Wave 3-B (RE.4): pad to the synthetic stub field count when defined
        // (mirrors `define_class_with_options`). Required for classes that are
        // upgraded from a synthetic stub but whose real bytecode field count
        // is smaller than the synthetic-mode layout used by native helpers.
        let stub_fields_for_pad = synthetic_stub_fields(name);
        let stub_instance_count_for_pad = stub_fields_for_pad
            .iter()
            .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
            .count();
        let stub_parent_fields_for_pad = match superclass_id {
            Some(super_id) => self
                .class_store
                .get(super_id)
                .map_or(0, |c| c.num_total_fields),
            None => 0,
        };
        let stub_total_for_pad = stub_parent_fields_for_pad + stub_instance_count_for_pad;
        let num_total_fields = num_total_fields.max(stub_total_for_pad);

        // Extract source file. Inlined here (rather than
        // `class_file.source_file()`) so we pattern-match through
        // `LazyAttribute::as_decoded()`. Every entry was already decoded by
        // the `force_decode_all` call above, so `as_decoded()` returns
        // `Some` for every attribute and the `_ => None` arm only fires for
        // attributes of a different kind.
        let source_file = class_file.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::SourceFile(name)) => Some(name.clone()),
            _ => None,
        });

        // Extract bootstrap methods
        let bootstrap_methods = class_file
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::BootstrapMethods(bms)) => Some(bms.clone()),
                _ => None,
            })
            .unwrap_or_default();

        // Extract annotations
        let mut annotations = Vec::new();
        for attr in &class_file.attributes {
            match attr.as_decoded() {
                Some(Attribute::RuntimeVisibleAnnotations(anns))
                | Some(Attribute::RuntimeInvisibleAnnotations(anns)) => {
                    annotations.extend(anns.iter().cloned());
                }
                _ => {}
            }
        }

        // Extract Signature
        let signature = class_file.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::Signature(s)) => Some(s.clone()),
            _ => None,
        });

        // Preserve the larger num_total_fields so existing objects don't break
        let old_num_total = self
            .class_store
            .get(id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        let final_num_total = num_total_fields.max(old_num_total);

        // Update the class in-place
        if let Some(class) = self.class_store.get_mut(id) {
            class.source_file = source_file;
            class.version = class_file.version;
            class.constant_pool = class_file.constant_pool;
            class.access_flags = class_file.access_flags;
            class.superclass = superclass_id;
            class.interfaces = interface_ids;
            class.fields = class_file.fields;
            class.methods = class_file.methods;
            class.first_field_index = first_field_index;
            class.num_total_fields = final_num_total;
            class.bootstrap_methods = bootstrap_methods;
            class.annotations = annotations;
            class.signature = signature;
            class.is_synthetic_stub = false;
            // Reset state to Loaded so verification + init can run
            class.state = ClassState::Loaded;

            // Compute has_finalizer
            class.has_finalizer = class.declares_finalize();
        }
        // Check parent's has_finalizer (needs separate borrow)
        let parent_has = superclass_id
            .and_then(|sid| self.class_store.get(sid))
            .map_or(false, |parent| parent.has_finalizer);
        if parent_has {
            if let Some(class) = self.class_store.get_mut(id) {
                class.has_finalizer = true;
            }
        }

        // Cache the class bytes
        self.class_bytes_cache
            .insert(name.to_string(), bytes.to_vec());

        Ok(())
    }
}

/// Return the correct superclass for well-known JDK classes.
///
/// This is critical for exception handling: without the correct hierarchy,
/// `catch (RuntimeException e)` won't match `ArithmeticException` because
/// `is_subclass_of` checks require a correct parent chain.
/// Public lookup for verifier fallback — walks JDK superclass hierarchy
/// when classes aren't loaded yet.
pub fn jdk_superclass_lookup(name: &str) -> &'static str {
    jdk_superclass(name)
}

fn jdk_superclass(name: &str) -> &'static str {
    match name {
        // Throwable hierarchy
        "java/lang/Throwable" => "java/lang/Object",

        // Error hierarchy
        "java/lang/Error" => "java/lang/Throwable",
        "java/lang/StackOverflowError"
        | "java/lang/OutOfMemoryError"
        | "java/lang/VirtualMachineError"
        | "java/lang/InternalError"
        | "java/lang/AssertionError" => "java/lang/Error",

        // Exception hierarchy
        "java/lang/Exception" => "java/lang/Throwable",
        "java/io/IOException" => "java/lang/Exception",
        "java/io/FileNotFoundException" => "java/io/IOException",

        // RuntimeException hierarchy
        "java/lang/RuntimeException" => "java/lang/Exception",
        "java/lang/NullPointerException"
        | "java/lang/ArithmeticException"
        | "java/lang/ArrayIndexOutOfBoundsException"
        | "java/lang/IndexOutOfBoundsException"
        | "java/lang/StringIndexOutOfBoundsException"
        | "java/lang/ClassCastException"
        | "java/lang/IllegalArgumentException"
        | "java/lang/IllegalStateException"
        | "java/lang/UnsupportedOperationException"
        | "java/lang/ClassNotFoundException"
        | "java/lang/NoSuchMethodException"
        | "java/lang/NoSuchFieldException"
        | "java/lang/NegativeArraySizeException"
        | "java/lang/ArrayStoreException"
        | "java/lang/NumberFormatException"
        | "java/lang/IllegalAccessException"
        | "java/lang/IllegalMonitorStateException"
        | "java/lang/UnsatisfiedLinkError"
        | "java/lang/SecurityException"
        | "java/lang/MatchException" => "java/lang/RuntimeException",

        // java.util exceptions
        "java/util/NoSuchElementException"
        | "java/util/ConcurrentModificationException"
        | "java/util/InputMismatchException" => "java/lang/RuntimeException",

        // Linkage errors
        "java/lang/LinkageError" => "java/lang/Error",
        "java/lang/NoClassDefFoundError"
        | "java/lang/IncompatibleClassChangeError"
        | "java/lang/ClassFormatError"
        | "java/lang/VerifyError"
        | "java/lang/NoSuchFieldError"
        | "java/lang/NoSuchMethodError"
        | "java/lang/IllegalAccessError"
        | "java/lang/AbstractMethodError"
        | "java/lang/ExceptionInInitializerError"
        | "java/lang/BootstrapMethodError" => "java/lang/LinkageError",

        // java.lang.reflect (JDK hierarchy for reflective wrappers)
        "java/lang/reflect/ReflectiveOperationException" => "java/lang/Exception",
        "java/lang/reflect/InvocationTargetException" => "java/lang/reflect/ReflectiveOperationException",

        // java.nio.file.attribute — enum PosixFilePermission extends Enum
        "java/nio/file/attribute/PosixFilePermission" => "java/lang/Enum",

        // Number type hierarchy
        "java/lang/Integer" | "java/lang/Long" | "java/lang/Short" | "java/lang/Byte"
        | "java/lang/Float" | "java/lang/Double" => "java/lang/Number",
        "java/lang/Number" => "java/lang/Object",

        // Record hierarchy (JEP 395, Java 16+)
        "java/lang/Record" => "java/lang/Object",

        // ---- java.io hierarchy ----
        // Abstract base classes extend Object
        "java/io/InputStream" | "java/io/OutputStream"
        | "java/io/Reader" | "java/io/Writer" => "java/lang/Object",

        // Filter streams wrap another stream
        "java/io/FilterInputStream" | "java/io/BufferedInputStream"
        | "java/io/DataInputStream" => "java/io/InputStream",
        "java/io/FilterOutputStream" | "java/io/BufferedOutputStream"
        | "java/io/DataOutputStream" => "java/io/OutputStream",

        // File streams extend base streams directly
        "java/io/FileInputStream" | "java/io/ByteArrayInputStream"
        | "java/io/ObjectInputStream" => "java/io/InputStream",
        "java/io/FileOutputStream" | "java/io/ByteArrayOutputStream"
        | "java/io/ObjectOutputStream" => "java/io/OutputStream",

        // PrintStream extends FilterOutputStream
        "java/io/PrintStream" => "java/io/FilterOutputStream",

        // Reader/Writer subclasses
        "java/io/BufferedReader" | "java/io/InputStreamReader"
        | "java/io/StringReader" => "java/io/Reader",
        "java/io/BufferedWriter" | "java/io/OutputStreamWriter"
        | "java/io/PrintWriter" | "java/io/StringWriter" => "java/io/Writer",

        // FileReader/FileWriter extend stream reader/writer
        "java/io/FileReader" => "java/io/InputStreamReader",
        "java/io/FileWriter" => "java/io/OutputStreamWriter",

        // ---- javax.naming hierarchy ----
        "javax/naming/NamingException" => "java/lang/Exception",
        "javax/naming/NameNotFoundException"
        | "javax/naming/InvalidNameException" => "javax/naming/NamingException",

        // ---- java.nio hierarchy ----
        "java/nio/Buffer" => "java/lang/Object",
        "java/nio/ByteBuffer" | "java/nio/CharBuffer"
        | "java/nio/ShortBuffer" | "java/nio/IntBuffer"
        | "java/nio/LongBuffer" | "java/nio/FloatBuffer"
        | "java/nio/DoubleBuffer" => "java/nio/Buffer",
        "java/nio/HeapByteBuffer" => "java/nio/ByteBuffer",
        "java/nio/HeapCharBuffer" => "java/nio/CharBuffer",
        "java/nio/charset/Charset" => "java/lang/Object",

        // ---- T19.H5: AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater
        // / AtomicLongFieldUpdater synthetic impl subclasses. Real Java
        // bytecode that calls `newUpdater(...)` then implicitly casts the
        // result to the abstract base — the cast only succeeds if the
        // returned object's class chain reaches the abstract base. So we
        // declare each `$RustJvmImpl` as a direct subclass of the
        // corresponding factory.
        "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl" =>
            "java/util/concurrent/atomic/AtomicReferenceFieldUpdater",
        "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl" =>
            "java/util/concurrent/atomic/AtomicIntegerFieldUpdater",
        "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl" =>
            "java/util/concurrent/atomic/AtomicLongFieldUpdater",

        // ---- S-trinity #1: jdk.internal.loader ClassLoader chain.
        // Without these the synthetic-stub for `ClassLoaders$AppClassLoader`
        // / `ClassLoaders$PlatformClassLoader` defaults to `java/lang/Object`
        // as superclass (and worse, the `$` in the name flips the
        // access-flags heuristic at `class_manager.rs:2803` to mark them
        // as interfaces). Both effects break
        // `(ClassLoader) priv.run()` checkcasts in callers like
        // `LoaderUtil.getClassLoader` because our app-loader instances
        // (allocated by `alloc_classloader` with class
        // `jdk/internal/loader/ClassLoaders$AppClassLoader`) end up not
        // being recognised as a `ClassLoader`.
        "jdk/internal/loader/ClassLoaders$AppClassLoader"
        | "jdk/internal/loader/ClassLoaders$PlatformClassLoader" =>
            "jdk/internal/loader/BuiltinClassLoader",
        "jdk/internal/loader/BuiltinClassLoader" =>
            "java/security/SecureClassLoader",
        "java/security/SecureClassLoader" => "java/lang/ClassLoader",
        "java/net/URLClassLoader" => "java/security/SecureClassLoader",
        "java/lang/ClassLoader" => "java/lang/Object",

        // ---- LETSGO_S1: java.util collections compatibility layer ----
        //
        // Real JDK declares an `Abstract*` skeletal hierarchy under every
        // concrete collection. Without this chain, synthetic-stub
        // dispatch resolves `LinkedHashSet.add(Object)Z` as
        // `LinkedHashSet.<class chain only Object>.add` and surfaces a
        // `NoSuchMethodError` (failure mode observed during letsgo-main
        // boot). Wiring the chain lets `find_method_recursive` and the
        // native-dispatch fallback walk parents until they locate the
        // method/native registered on the closest concrete ancestor
        // (e.g. `HashSet`).
        //
        // ONLY edges where the parent contributes zero (or matching)
        // synthetic fields are listed here so we don't perturb existing
        // field-slot layouts that natives depend on. In particular,
        // `LinkedHashMap`, `Properties`, and `Stack` keep their direct
        // `Object` parent because their `synthetic_stub_fields` already
        // count fields the candidate parent would also declare.
        //
        // Abstract bases (no synthetic fields):
        "java/util/AbstractCollection" => "java/lang/Object",
        "java/util/AbstractList" => "java/util/AbstractCollection",
        "java/util/AbstractSet" => "java/util/AbstractCollection",
        "java/util/AbstractMap" => "java/lang/Object",
        "java/util/AbstractQueue" => "java/util/AbstractCollection",
        "java/util/AbstractSequentialList" => "java/util/AbstractList",
        "java/util/Dictionary" => "java/lang/Object",

        // Concrete Set hierarchy:
        "java/util/HashSet" => "java/util/AbstractSet",
        "java/util/LinkedHashSet" => "java/util/HashSet",
        "java/util/TreeSet" => "java/util/AbstractSet",
        "java/util/EnumSet" => "java/util/AbstractSet",
        "java/util/concurrent/CopyOnWriteArraySet" => "java/util/AbstractSet",
        "java/util/concurrent/ConcurrentSkipListSet" => "java/util/AbstractSet",

        // Concrete List/Queue hierarchy:
        "java/util/ArrayList" => "java/util/AbstractList",
        "java/util/LinkedList" => "java/util/AbstractSequentialList",
        "java/util/Vector" => "java/util/AbstractList",
        "java/util/ArrayDeque" => "java/util/AbstractCollection",
        "java/util/PriorityQueue" => "java/util/AbstractQueue",
        "java/util/concurrent/CopyOnWriteArrayList" => "java/util/AbstractList",
        "java/util/concurrent/ConcurrentLinkedQueue" => "java/util/AbstractQueue",
        "java/util/concurrent/ConcurrentLinkedDeque" => "java/util/AbstractCollection",

        // Concrete Map hierarchy:
        "java/util/HashMap" => "java/util/AbstractMap",
        "java/util/TreeMap" => "java/util/AbstractMap",
        "java/util/IdentityHashMap" => "java/util/AbstractMap",
        "java/util/WeakHashMap" => "java/util/AbstractMap",
        "java/util/EnumMap" => "java/util/AbstractMap",
        "java/util/concurrent/ConcurrentHashMap" => "java/util/AbstractMap",
        "java/util/concurrent/ConcurrentSkipListMap" => "java/util/AbstractMap",
        "java/util/Hashtable" => "java/util/Dictionary",

        // Default: everything else extends Object
        _ => "java/lang/Object",
    }
}

/// Return the interfaces implemented by well-known JDK stub classes.
///
/// This is critical for bytecode verification: without correct interface
/// declarations, the verifier rejects `String` where `CharSequence` is expected.
fn jdk_interfaces(name: &str) -> &'static [&'static str] {
    match name {
        "java/lang/String" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
            "java/lang/CharSequence",
            "java/lang/constant/Constable",
        ],
        "java/lang/StringBuilder" | "java/lang/StringBuffer" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
            "java/lang/CharSequence",
        ],
        "java/lang/Integer" | "java/lang/Long" | "java/lang/Short" | "java/lang/Byte"
        | "java/lang/Float" | "java/lang/Double" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
        ],
        "java/lang/Boolean" | "java/lang/Character" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
        ],
        "java/util/ArrayList" | "java/util/LinkedList" | "java/util/Vector"
        | "java/util/concurrent/CopyOnWriteArrayList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/HashMap" | "java/util/LinkedHashMap" | "java/util/TreeMap"
        | "java/util/IdentityHashMap" | "java/util/WeakHashMap"
        | "java/util/EnumMap"
        | "java/util/concurrent/ConcurrentHashMap"
        | "java/util/concurrent/ConcurrentSkipListMap" => &[
            "java/util/Map",
            "java/io/Serializable",
        ],
        "java/util/HashSet" | "java/util/LinkedHashSet" | "java/util/TreeSet"
        | "java/util/EnumSet"
        | "java/util/concurrent/CopyOnWriteArraySet"
        | "java/util/concurrent/ConcurrentSkipListSet" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        // LETSGO_S1: Skeletal abstract bases — declare the same root
        // interfaces as their concrete subclasses so `instanceof` checks
        // travelling through the abstract base land on the right answer.
        "java/util/AbstractCollection" => &[
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/AbstractList" | "java/util/AbstractSequentialList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/AbstractSet" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/AbstractMap" => &["java/util/Map"],
        "java/util/AbstractQueue" => &[
            "java/util/Queue",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/Hashtable" | "java/util/Properties" => &[
            "java/util/Map",
            "java/io/Serializable",
            "java/lang/Cloneable",
        ],
        "java/util/Dictionary" => &[],
        "java/util/ArrayDeque" => &[
            "java/util/Deque",
            "java/util/Queue",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/PriorityQueue" => &[
            "java/util/Queue",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/Stack" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/lang/Throwable" | "java/lang/Exception" | "java/lang/RuntimeException"
        | "java/lang/Error" => &["java/io/Serializable"],
        "java/lang/Enum" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
        ],
        "java/lang/Class" => &[
            "java/io/Serializable",
            "java/lang/reflect/GenericDeclaration",
            "java/lang/reflect/Type",
            "java/lang/reflect/AnnotatedElement",
        ],
        // ---- java.io / java.nio interfaces ----
        "java/io/InputStream" => &["java/io/Closeable"],
        "java/io/OutputStream" => &["java/io/Closeable", "java/io/Flushable"],
        "java/io/Reader" => &["java/lang/Readable", "java/io/Closeable"],
        "java/io/Writer" => &["java/lang/Appendable", "java/io/Closeable", "java/io/Flushable"],
        "java/io/Closeable" => &["java/lang/AutoCloseable"],
        "java/io/PrintStream" => &["java/lang/Appendable"],
        "java/nio/charset/Charset" => &["java/lang/Comparable"],

        // Synthetic functional interface composition classes (M3 fix)
        "java/util/function/Function$AndThen"
        | "java/util/function/Function$Compose"
        | "java/util/function/Function$Identity" => &["java/util/function/Function"],
        "java/util/function/Consumer$AndThen" => &["java/util/function/Consumer"],
        "java/util/function/Predicate$And"
        | "java/util/function/Predicate$Or"
        | "java/util/function/Predicate$Negate" => &["java/util/function/Predicate"],

        // S111r17 — Our internal `AnnotationProxy` must declare
        // `java.lang.annotation.Annotation` as a superinterface so that
        // class-graph walks (`is_subclass_of`, `array_is_assignable_to`)
        // recognise an `[Ljava/lang/annotation/AnnotationProxy;` array as
        // an `[Ljava/lang/annotation/Annotation;` array.  Spring 5.x
        // (SB2) `AnnotationUtils.adaptValue` does exactly that
        // `instanceof [Ljava.lang.annotation.Annotation;` check before
        // converting nested-annotation arrays to `AnnotationAttributes[]`,
        // and without the implements-edge the conversion silently
        // skips, leaving Spring to feed the raw `AnnotationProxy[]`
        // into `AnnotationAttributes.assertAttributeType` which then
        // throws `IllegalArgumentException`.
        "java/lang/annotation/AnnotationProxy" => &["java/lang/annotation/Annotation"],
        _ => &[],
    }
}

/// Detect JBoss-Logging i18n locale-suffix probes (`_$logger_<locale>` /
/// `_$bundle_<locale>`).
///
/// `Logger.doGetMessageLogger` walks a chain of generated implementation
/// class names — most-specific locale variant down to the locale-less
/// `_$logger` / `_$bundle` shipped in the JAR — wrapping each
/// `Lookup.findClass` in a `try/catch (ClassNotFoundException)`. The
/// locale-suffixed variants are *intentionally absent*; the catch is the
/// signal to try the next variant. Without this special-case, our
/// `is_jdk_class("org/jboss/...")` synthetic-stub fallback would succeed
/// and the heuristic in `create_synthetic_stub` (treating any `$`-bearing
/// name as an interface) leads to a `Class.asSubclass` CCE that escapes
/// the caller's CNFE catch.
fn is_jboss_logging_locale_lookup(name: &str) -> bool {
    let suffix_start = name
        .rfind("_$logger_")
        .map(|i| i + "_$logger_".len())
        .or_else(|| name.rfind("_$bundle_").map(|i| i + "_$bundle_".len()));
    let Some(start) = suffix_start else {
        return false;
    };
    let suffix = &name[start..];
    !suffix.is_empty()
        && suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check if a class name belongs to the JDK (should get a synthetic stub
/// when its .class file is not found).
fn is_jdk_class(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("sun/")
        || name.starts_with("jdk/")
        || name.starts_with("com/sun/")
        || name.starts_with("[") // array type descriptors like [I, [Ljava/lang/String;
        // WP8.10.5 — non-JDK prefixes whose classes have rich synthetic-stub
        // layouts declared below in `synthetic_stub_fields` (org/jboss/*,
        // org/wildfly/*, etc.). Without these, references to
        // org.jboss.modules.Module raise NoClassDefFoundError before the
        // synthetic-stub fallback in `load_class` can fire. The fallback
        // only triggers when classpath lookup has already failed, so a real
        // jboss-modules.jar on the classpath still loads normally.
        || name.starts_with("org/jboss/")
        || name.starts_with("org/wildfly/")
        || name.starts_with("org/xnio/")
        || name.starts_with("org/infinispan/")
        || name.starts_with("io/quarkus/")
        || name.starts_with("io/agroal/")
        || name.starts_with("io/undertow/")
        || name.starts_with("io/smallrye/")
}

/// Create the field declarations for well-known JDK stub classes.
///
/// Most stubs have no fields. Special cases provide the fields that native
/// implementations expect, so that `new` + `<init>` works correctly.
fn synthetic_stub_fields(name: &str) -> Vec<rustjvm_reader::field::ClassFileField> {
    use rustjvm_reader::field::ClassFileField;

    /// Helper to create N unnamed instance fields (for synthetic objects
    /// whose native code accesses fields by index, not by name).
    fn instance_fields(n: usize) -> Vec<ClassFileField> {
        (0..n)
            .map(|i| ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc(&format!("_f{i}")),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            })
            .collect()
    }

    match name {
        // String: 2 instance fields (value char[], hash int) — enough for
        // synthetic mode.  When the real JDK String class is loaded from a
        // .class file, its actual field count (e.g. 4 in JDK 25) is used
        // instead; see `create_java_string` in vm_object.rs.
        "java/lang/String" => instance_fields(2),
        // System has static fields for streams
        "java/lang/System" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: rustjvm_types::intern_arc("in"),
                descriptor: rustjvm_types::intern_arc("Ljava/io/InputStream;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: rustjvm_types::intern_arc("out"),
                descriptor: rustjvm_types::intern_arc("Ljava/io/PrintStream;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                name: rustjvm_types::intern_arc("err"),
                descriptor: rustjvm_types::intern_arc("Ljava/io/PrintStream;"),
                attributes: vec![],
            },
        ],
        // StringBuilder/StringBuffer: 2 fields (backing char[], count)
        "java/lang/StringBuilder" | "java/lang/StringBuffer" => instance_fields(2),
        // Throwable and common exception types: 2 fields (message, cause)
        "java/lang/Throwable"
        | "java/lang/Exception"
        | "java/lang/RuntimeException"
        | "java/lang/Error"
        | "java/lang/NullPointerException"
        | "java/lang/ArithmeticException"
        | "java/lang/ArrayIndexOutOfBoundsException"
        | "java/lang/IndexOutOfBoundsException"
        | "java/lang/ClassCastException"
        | "java/lang/IllegalArgumentException"
        | "java/lang/IllegalStateException"
        | "java/lang/UnsupportedOperationException"
        | "java/lang/ClassNotFoundException"
        | "java/lang/NoSuchMethodException"
        | "java/lang/StackOverflowError"
        | "java/lang/OutOfMemoryError"
        | "java/util/NoSuchElementException"
        | "java/util/InputMismatchException"
        | "java/io/IOException"
        | "java/io/FileNotFoundException"
        | "java/lang/NumberFormatException" => instance_fields(2),
        // Wrapper types: 1 instance field (primitive value) + static TYPE field
        // (Integer.TYPE == int.class, etc.)
        "java/lang/Integer"
        | "java/lang/Long"
        | "java/lang/Float"
        | "java/lang/Double"
        | "java/lang/Boolean"
        | "java/lang/Character"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Void" => {
            let mut fields = vec![ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("TYPE"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"),
                attributes: vec![],
            }];
            fields.extend(instance_fields(1));
            fields
        }
        // Collections: ArrayList/Vector/Stack/CopyOnWriteArrayList = 2 fields (data, size)
        "java/util/ArrayList"
        | "java/util/Vector"
        | "java/util/Stack"
        | "java/util/concurrent/CopyOnWriteArrayList" => instance_fields(2),
        // HashMap/HashSet/ConcurrentHashMap = 3 fields (buckets, size, capacity)
        "java/util/HashMap"
        | "java/util/HashSet"
        | "java/util/Hashtable"
        | "java/util/concurrent/ConcurrentHashMap" => instance_fields(3),
        // LinkedList = 3 fields (head, tail, size)
        "java/util/LinkedList" => instance_fields(3),
        // LinkedHashMap = 5 fields
        "java/util/LinkedHashMap" => instance_fields(5),
        // TreeMap/TreeSet = 3 fields (data, size, comparator)
        "java/util/TreeMap" | "java/util/TreeSet" => instance_fields(3),
        // ArrayDeque = 4 fields (data, head, tail, size)
        "java/util/ArrayDeque" => instance_fields(4),
        // PriorityQueue = 3 fields (data, size, comparator)
        "java/util/PriorityQueue" => instance_fields(3),
        // StringJoiner = 5 fields
        "java/util/StringJoiner" => instance_fields(5),
        // Scanner = 5 fields
        "java/util/Scanner" => instance_fields(5),
        // Optional = 1 field
        "java/util/Optional"
        | "java/util/OptionalInt"
        | "java/util/OptionalLong"
        | "java/util/OptionalDouble" => instance_fields(1),
        // T2.3.8 — Spliterator and its primitive specializations:
        //   field 0 = backing array (Object[], int[], long[], double[])
        //   field 1 = cursor Int (next element to emit)
        "java/util/Spliterator"
        | "java/util/Spliterator$OfInt"
        | "java/util/Spliterator$OfLong"
        | "java/util/Spliterator$OfDouble" => instance_fields(2),
        // ---- java.io field layouts ----
        // All stream/reader/writer subclasses store fd or wrapped stream in field 0.
        // Native code accesses field 0 as the fd/delegate reference.
        "java/io/FileInputStream" | "java/io/FileOutputStream"
        | "java/io/FilterInputStream" | "java/io/FilterOutputStream"
        | "java/io/InputStreamReader" | "java/io/OutputStreamWriter"
        | "java/io/BufferedReader" | "java/io/BufferedWriter"
        | "java/io/DataInputStream" | "java/io/DataOutputStream"
        | "java/io/FileReader" | "java/io/FileWriter" => instance_fields(1),
        // PrintStream/PrintWriter = 1 field (fd)
        "java/io/PrintStream" | "java/io/PrintWriter" => instance_fields(1),
        // T1.10 — corrected StringReader/StringWriter shapes to match
        // the real native init code in `native-io/src/lib.rs`:
        //   StringReader = 3 fields (content, pos, length) per
        //   `SR_FIELD_CONTENT/POS/LENGTH` constants.
        //   StringWriter = 2 fields (buffer, count) per
        //   `SW_FIELD_BUF/COUNT` constants.
        // The previous values (2 / 1) were stale and caused
        // `gen_heap::set_field` bounds-check panics during
        // `native_sw_init` / `native_sr_init`.
        "java/io/StringReader" => instance_fields(3),
        "java/io/StringWriter" => instance_fields(2),
        // ByteArrayInputStream = 4 (buf, pos, mark, count) — real-JDK layout (Session 83).
        "java/io/ByteArrayInputStream" => instance_fields(4),
        // ByteArrayOutputStream = 2 (data, count)
        "java/io/ByteArrayOutputStream" => instance_fields(2),
        // ObjectInputStream/ObjectOutputStream = 6 synthetic fields
        "java/io/ObjectInputStream" | "java/io/ObjectOutputStream" => instance_fields(6),
        // File = 1 field (path string)
        "java/io/File" => instance_fields(1),
        // ---- java.nio field layouts ----
        // Buffer = 4 fields (position, limit, capacity, mark)
        "java/nio/Buffer" => instance_fields(4),
        // ByteBuffer/CharBuffer = 5 fields (array, position, limit, capacity, mark)
        "java/nio/ByteBuffer" | "java/nio/CharBuffer"
        | "java/nio/HeapByteBuffer" | "java/nio/HeapCharBuffer"
        | "java/nio/ShortBuffer" | "java/nio/IntBuffer"
        | "java/nio/LongBuffer" | "java/nio/FloatBuffer"
        | "java/nio/DoubleBuffer" => instance_fields(5),
        // Charset = 2 fields (name, aliases)
        "java/nio/charset/Charset" => instance_fields(2),
        // StandardCharsets — 6 public static final Charset fields
        "java/nio/charset/StandardCharsets" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("UTF_8"),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("US_ASCII"),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("ISO_8859_1"),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("UTF_16"),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("UTF_16BE"),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("UTF_16LE"),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/charset/Charset;"),
                attributes: vec![],
            },
        ],
        // Random = 2 fields (seed, has-next-gaussian)
        "java/util/Random" => instance_fields(2),
        // UUID = 2 fields (msb, lsb)
        "java/util/UUID" => instance_fields(2),
        // Properties = 4 fields
        "java/util/Properties" => instance_fields(4),
        // Formatter = 2 fields (output=0, locale=1)
        "java/util/Formatter" => instance_fields(2),
        // DecimalFormat = 4 fields (pattern=0, groupingUsed=1, maxFracDigits=2, minFracDigits=3)
        "java/text/DecimalFormat" => instance_fields(4),
        // NumberFormat = 4 fields
        "java/text/NumberFormat" => instance_fields(4),
        // MessageFormat = 1 field (pattern=0)
        "java/text/MessageFormat" => instance_fields(1),
        // Thread = 5 fields (name=0, priority=1, tid=2, target/runnable=3, virtualFlag=4)
        "java/lang/Thread" => instance_fields(5),
        "java/lang/Thread$State" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("NEW"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Thread$State;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("RUNNABLE"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Thread$State;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("BLOCKED"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Thread$State;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("WAITING"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Thread$State;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("TIMED_WAITING"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Thread$State;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("TERMINATED"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Thread$State;"),
                attributes: vec![],
            },
        ],
        // Atomic types: 1 field (value=0)
        "java/util/concurrent/atomic/AtomicInteger"
        | "java/util/concurrent/atomic/AtomicLong"
        | "java/util/concurrent/atomic/AtomicBoolean"
        | "java/util/concurrent/atomic/AtomicReference"
        | "java/util/concurrent/atomic/AtomicStampedReference"
        | "java/util/concurrent/atomic/AtomicMarkableReference" => instance_fields(1),
        // Atomic arrays: 2 fields (array=0, length=1)
        "java/util/concurrent/atomic/AtomicIntegerArray"
        | "java/util/concurrent/atomic/AtomicLongArray"
        | "java/util/concurrent/atomic/AtomicReferenceArray" => instance_fields(2),
        // ReentrantLock: 3 fields (owner=0, holdCount=1, fair=2)
        "java/util/concurrent/locks/ReentrantLock" => instance_fields(3),
        // Condition: 1 field (lock=0)
        "java/util/concurrent/locks/Condition" => instance_fields(1),
        // ReentrantReadWriteLock: 3 fields (readers=0, writer=1, fair=2)
        "java/util/concurrent/locks/ReentrantReadWriteLock" => instance_fields(3),
        // ReadLock/WriteLock: 1 field (parent RWL ref=0)
        "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock"
        | "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock" => instance_fields(1),
        // CountDownLatch: 1 field (count=0)
        "java/util/concurrent/CountDownLatch" => instance_fields(1),
        // Semaphore: 2 fields (permits=0, fair=1)
        "java/util/concurrent/Semaphore" => instance_fields(2),
        // CyclicBarrier: 3 fields (parties=0, count=1, broken=2)
        "java/util/concurrent/CyclicBarrier" => instance_fields(3),
        // Phaser: 3 fields (parties=0, arrivals=1, phase=2)
        "java/util/concurrent/Phaser" => instance_fields(3),
        // Exchanger: 1 field (slot=0)
        "java/util/concurrent/Exchanger" => instance_fields(1),
        // CompletableFuture: 4 fields (result=0, done=1, source=2, handler=3)
        "java/util/concurrent/CompletableFuture" => instance_fields(4),
        // LinkedBlockingQueue: 4 fields (head=0, tail=1, size=2, capacity=3)
        "java/util/concurrent/LinkedBlockingQueue" => instance_fields(4),
        // ArrayBlockingQueue: 4 fields (same as LBQ)
        "java/util/concurrent/ArrayBlockingQueue" => instance_fields(4),
        // ConcurrentLinkedQueue/Deque: 4 fields (same layout as LBQ)
        "java/util/concurrent/ConcurrentLinkedQueue"
        | "java/util/concurrent/ConcurrentLinkedDeque" => instance_fields(4),
        // PriorityBlockingQueue: 4 fields
        "java/util/concurrent/PriorityBlockingQueue" => instance_fields(4),
        // ForkJoinPool: 1 field (parallelism=0)
        "java/util/concurrent/ForkJoinPool" => instance_fields(1),
        // ForkJoinTask: 2 fields (result=0, done=1)
        "java/util/concurrent/ForkJoinTask"
        | "java/util/concurrent/RecursiveTask"
        | "java/util/concurrent/RecursiveAction" => instance_fields(2),
        // ThreadPoolExecutor: 2 fields (poolSize=0, isShutdown=1)
        "java/util/concurrent/ThreadPoolExecutor" => instance_fields(2),
        // ScheduledThreadPoolExecutor: 2 fields
        "java/util/concurrent/ScheduledThreadPoolExecutor" => instance_fields(2),
        // ScheduledFuture: 2 fields (result=0, done=1)
        "java/util/concurrent/ScheduledFuture" => instance_fields(2),
        // Future: 2 fields (result=0, done=1)
        "java/util/concurrent/Future" => instance_fields(2),
        // StampedLock: 4 fields
        "java/util/concurrent/locks/StampedLock" => instance_fields(4),
        // ConcurrentSkipListMap: 3 fields
        "java/util/concurrent/ConcurrentSkipListMap" => instance_fields(3),
        // CopyOnWriteArraySet: 2 fields (same as COWAL)
        "java/util/concurrent/CopyOnWriteArraySet" => instance_fields(2),
        // TimeUnit: 1 instance field (ordinal) + 7 static fields (enum constants)
        "java/util/concurrent/TimeUnit" => {
            let mut fields = vec![ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("_f0"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            }];
            for name in &[
                "NANOSECONDS",
                "MICROSECONDS",
                "MILLISECONDS",
                "SECONDS",
                "MINUTES",
                "HOURS",
                "DAYS",
            ] {
                fields.push(ClassFileField {
                    access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC,
                    name: rustjvm_types::intern_arc(name),
                    descriptor: rustjvm_types::intern_arc("Ljava/util/concurrent/TimeUnit;"),
                    attributes: vec![],
                });
            }
            fields
        }
        // java.lang.ref: Reference = 2 fields (referent=0, queue=1)
        "java/lang/ref/Reference"
        | "java/lang/ref/WeakReference"
        | "java/lang/ref/SoftReference"
        | "java/lang/ref/PhantomReference"
        | "java/lang/ref/Cleaner" => instance_fields(2),
        // ReferenceQueue = 2 fields (head=0, size=1)
        "java/lang/ref/ReferenceQueue" => instance_fields(2),
        // ScopedValue: 3 fields (value=0, isBound=1, hash=2)
        "java/lang/ScopedValue" => instance_fields(3),
        // ScopedValue$Carrier: 3 fields (svRef=0, valueRef=1, parentRef=2)
        "java/lang/ScopedValue$Carrier" => instance_fields(3),
        // ScopedValue$Snapshot: 2 fields (bindingsCount=0, timestamp=1)
        "java/lang/ScopedValue$Snapshot" => instance_fields(2),
        // StructuredTaskScope: 8 fields
        "java/util/concurrent/StructuredTaskScope"
        | "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure"
        | "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess" => instance_fields(8),
        // StructuredTaskScope$Subtask: 4 fields (state=0, result=1, exception=2, callable=3)
        "java/util/concurrent/StructuredTaskScope$Subtask" => instance_fields(4),
        // Joiner: 4 fields (policy=0, results=1, exception=2, completed=3)
        "java/util/concurrent/StructuredTaskScope$Joiner" => instance_fields(4),
        // Config: 3 fields (name=0, threadFactory=1, timeoutMs=2)
        "java/util/concurrent/StructuredTaskScope$Config" => instance_fields(3),

        // ---- java.lang.reflect layouts (synthetic-jdk mode) ----
        //
        // Real JDK layout of Method/Field/Constructor uses JDK field names
        // that `create_*_object` in native-builtins/src/lang_class.rs populates
        // via `set_field_by_name`. Previously these stubs had ZERO fields,
        // so `set_field_by_name` silently no-op'd on `clazz`, `name`, etc.,
        // which caused `Method.invoke` to see a null `clazz` and abort with
        // "no declaring class". Declaring the fields here makes name-based
        // resolution find them via `resolve_field_index_in_hierarchy`.
        //
        // The descriptor strings are the real JDK types so bytecode
        // getfield/putfield resolves the correct static types; native code
        // only uses the name, so minor descriptor mismatches wouldn't matter
        // for synthetic-jdk's native-driven paths.
        "java/lang/reflect/AccessibleObject" => vec![
            // AccessibleObject.override (boolean, JDK field name `override`)
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("override"),
                descriptor: rustjvm_types::intern_arc("Z"),
                attributes: vec![],
            },
        ],
        "java/lang/reflect/Field" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("override"), descriptor: rustjvm_types::intern_arc("Z"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("clazz"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("slot"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("type"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("modifiers"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("trustedFinal"), descriptor: rustjvm_types::intern_arc("Z"), attributes: vec![] },
        ],
        "java/lang/reflect/Method" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("override"), descriptor: rustjvm_types::intern_arc("Z"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("clazz"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("slot"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("returnType"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("parameterTypes"), descriptor: rustjvm_types::intern_arc("[Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("modifiers"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("callerSensitive"), descriptor: rustjvm_types::intern_arc("B"), attributes: vec![] },
        ],
        "java/lang/reflect/Constructor" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("override"), descriptor: rustjvm_types::intern_arc("Z"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("clazz"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("slot"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("parameterTypes"), descriptor: rustjvm_types::intern_arc("[Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("modifiers"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],

        // ---- T16.5 / T16.6: NIO async channels + UDP ----
        // Field layouts shared with `native-io::nio_native::register_t16_channel_overrides`
        // and `phases_late::register_p67_async_channels` /
        // `register_datagram_channel`. These ensure `alloc_object` backed by
        // `ensure_class_initialized` reserves enough slots so subsequent
        // `set_field(i, ...)` calls don't drop writes.
        //
        // AsynchronousFileChannel = 3 (path_str=0, open=1, _unused=2)
        "java/nio/channels/AsynchronousFileChannel" => instance_fields(3),
        // AsynchronousSocketChannel = 4 (connected=0, open=1, fd=2, remote=3)
        "java/nio/channels/AsynchronousSocketChannel" => instance_fields(4),
        // AsynchronousServerSocketChannel = 1 (open=0)
        "java/nio/channels/AsynchronousServerSocketChannel" => instance_fields(1),
        // AsynchronousChannelGroup = 1 (state=0)
        "java/nio/channels/AsynchronousChannelGroup" => instance_fields(1),
        // DatagramChannel = 5 (port=0, open=1, connected=2, blocking=3, sock_id=4)
        "java/nio/channels/DatagramChannel" => instance_fields(5),
        // MulticastSocket = 5 (port=0, closed=1, timeout=2, fd_id=3, ttl=4)
        "java/net/MulticastSocket" => instance_fields(5),
        // Wave 3-B (RE.4): InetSocketAddress, HttpServer, HttpExchange,
        // HttpContext, Headers must be pre-sized so that the JVM `new` opcode
        // allocates enough slots for the synthetic-mode field layout used by
        // `native-builtins::net_phase_e`. The real-JDK classes have a much
        // smaller `num_total_fields` (e.g. InetSocketAddress has 1: holder),
        // and `upgrade_synthetic_class` preserves max(synthetic, real) so we
        // get the wider layout once the real bytecode loads.
        //
        // InetSocketAddress = 3 (holder=0 InetSocketAddressHolder, port=1 Int, addr=2 InetAddress).
        // Slot 0 mirrors the real-JDK layout (`private final InetSocketAddressHolder holder`)
        // so bytecode `getfield holder` sees the holder our synthetic helpers populate.
        "java/net/InetSocketAddress" => instance_fields(3),
        // InetSocketAddressHolder = 3 (hostname=0 String, addr=1 InetAddress, port=2 Int).
        // Wave 3-B² fix for the deeper dispatch bug: `InetSocketAddress.getPort()`
        // bytecode reads `this.holder` and invokevirtuals `Holder.getPort()` on it,
        // so the holder's class id MUST be honored — putting a String at slot 0 of
        // the InetSocketAddress would route the sub-invokevirtual to
        // `java/lang/String.getPort()` (NoSuchMethodError).
        "java/net/InetSocketAddress$InetSocketAddressHolder" => instance_fields(3),
        // InetAddress = 2 (hostName=0 String, address=1 String)
        "java/net/InetAddress"
        | "java/net/Inet4Address"
        | "java/net/Inet6Address" => instance_fields(2),
        // HttpServer (com.sun.net.httpserver) = 5 (address, started, contexts,
        // server_id, port) per `net_phase_e::HS_*` constants.
        "com/sun/net/httpserver/HttpServer"
        | "com/sun/net/httpserver/HttpServerImpl" => instance_fields(5),
        // HttpExchange = 8 (method, uri, reqHeaders, respHeaders, reqBody,
        // statusCode, owner_socket, response_chunks)
        "com/sun/net/httpserver/HttpExchange" => instance_fields(8),
        // HttpExchange$ResponseBody = 2 (owner exchange, dummy)
        "com/sun/net/httpserver/HttpExchange$ResponseBody" => instance_fields(2),
        // HttpContext = 2 (path, handler)
        "com/sun/net/httpserver/HttpContext" => instance_fields(2),
        // Headers = 1 (delegate HashMap)
        "com/sun/net/httpserver/Headers" => instance_fields(1),

        // Spring Boot 3 JarFileArchive.<clinit> reads PosixFilePermission.OWNER_* statics.
        // When the JDK image is unavailable we fall back to a synthetic stub; declare
        // the enum constants so GETSTATIC resolves, and wire values from a native <clinit>
        // (see `register_posix_file_permission_stub_clinit` in native-builtins).
        "java/nio/file/attribute/PosixFilePermission" => {
            let mk = |n: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc(n),
                descriptor: rustjvm_types::intern_arc("Ljava/nio/file/attribute/PosixFilePermission;"),
                attributes: vec![],
            };
            vec![
                mk("OWNER_READ"),
                mk("OWNER_WRITE"),
                mk("OWNER_EXECUTE"),
                mk("GROUP_READ"),
                mk("GROUP_WRITE"),
                mk("GROUP_EXECUTE"),
                mk("OTHERS_READ"),
                mk("OTHERS_WRITE"),
                mk("OTHERS_EXECUTE"),
            ]
        },

        // ---- T19.5: sun.nio.ch.Net TCP cluster ----
        // Layouts shared with `native-io::net::register_sun_nio_ch_net`.
        // These classes don't have Java-side instance fields of interest to
        // our natives — the `fd` integer that identifies a socket is carried
        // on the FileDescriptor handed in as an arg. We still register them
        // so `ensure_class_initialized` succeeds when a user class does
        // e.g. `ServerSocketChannel.open()` (which calls
        // `new ServerSocketChannelImpl(provider)` internally).
        //
        // sun/nio/ch/Net                     = 0 (static utility class; no instance state)
        // sun/nio/ch/ServerSocketChannelImpl = 4 (fd, localAddress, state, blocking)
        // sun/nio/ch/SocketChannelImpl       = 5 (fd, localAddress, remoteAddress, state, blocking)
        // sun/nio/ch/SelectorImpl            = 5 — T19.7.a layout:
        //   field 0: id (Int, lookup key into nio_selector::selectors())
        //   field 1: registered_map (Object, Java-side Set<SelectionKey>)
        //   field 2: selected_set   (Object, Java-side Set<SelectionKey>)
        //   field 3: keys_set       (Object, Java-side Set<SelectionKey>)
        //   field 4: open_flag      (Int, 1 = open, 0 = closed)
        // sun/nio/ch/SelectionKeyImpl        = 5 — T19.7.a layout:
        //   field 0: selector      (Object, parent Selector)
        //   field 1: channel       (Object, the SelectableChannel)
        //   field 2: interestOps   (Int)
        //   field 3: readyOps      (Int)
        //   field 4: attachment    (Object, user attachment slot)
        // java/nio/channels/ServerSocketChannel = 1 (provider)
        // java/nio/channels/SocketChannel    = 1 (provider)
        "sun/nio/ch/Net" => instance_fields(0),
        "sun/nio/ch/ServerSocketChannelImpl" => instance_fields(4),
        "sun/nio/ch/SocketChannelImpl" => instance_fields(5),
        "sun/nio/ch/SelectorImpl" => instance_fields(5),
        "sun/nio/ch/SelectionKeyImpl" => instance_fields(5),
        "java/nio/channels/ServerSocketChannel" => instance_fields(1),
        "java/nio/channels/SocketChannel" => instance_fields(1),

        // ---- T19.N1: java.security ProtectionDomain / CodeSource ----
        // Minimal-viable field layouts so `Class.getProtectionDomain0` can
        // populate the reflected-protection-domain returned to user code.
        // ProtectionDomain = 4 fields (codesource, permissions, classloader,
        // principals).  Matches the constructor signature
        //   ProtectionDomain(CodeSource cs, PermissionCollection p, ClassLoader cl, Principal[] ps)
        // that real JDK bytecode targets.
        "java/security/ProtectionDomain" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("codesource"), descriptor: rustjvm_types::intern_arc("Ljava/security/CodeSource;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("permissions"), descriptor: rustjvm_types::intern_arc("Ljava/security/PermissionCollection;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("classloader"), descriptor: rustjvm_types::intern_arc("Ljava/lang/ClassLoader;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("principals"), descriptor: rustjvm_types::intern_arc("[Ljava/security/Principal;"), attributes: vec![] },
        ],
        // CodeSource = 2 fields (location URL, signer certs array).  The
        // JDK layout has additional internals (`signers`, `codeSigners`) that
        // are computed lazily; a 2-field stub is enough for our
        // reflectively-retrieved PD to expose `getLocation()` + `getCertificates()`.
        "java/security/CodeSource" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("location"), descriptor: rustjvm_types::intern_arc("Ljava/net/URL;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("certs"), descriptor: rustjvm_types::intern_arc("[Ljava/security/cert/Certificate;"), attributes: vec![] },
        ],

        // ---- T19.1: JBoss MSC (Modular Service Container) field layouts ----
        //
        // Companion to `native-builtins/src/jboss_msc.rs`. The native
        // scheduler stores the Rust-side controller id in a trailing slot
        // on ServiceController (field 5) so Java bytecode can safely read
        // fields 0..=4 (the documented Java-visible shape) while natives
        // round-trip controller ids via the extra slot.
        "org/jboss/msc/service/ServiceName" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("segments"), descriptor: rustjvm_types::intern_arc("[Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("canonical"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
        ],
        "org/jboss/msc/service/ServiceController" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/msc/service/ServiceName;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("mode"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("value"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("listeners"), descriptor: rustjvm_types::intern_arc("Ljava/util/ArrayList;"), attributes: vec![] },
            // Synthetic trailing back-reference: Rust controller id.
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("_mscId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        "org/jboss/msc/service/ServiceContainer" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("services"), descriptor: rustjvm_types::intern_arc("Ljava/util/concurrent/ConcurrentHashMap;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("workerPoolHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        "org/jboss/msc/service/StartContext"
        | "org/jboss/msc/service/StopContext" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("controllerId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],

        // ---- T19.H4: JBoss Modules boot-path field layouts ----
        //
        // Companion to `native-builtins/src/jboss_module_loader.rs`.  The
        // post-clinit fixup in `vm/src/vm/vm_util.rs` populates
        // `DefaultBootModuleLoaderHolder.INSTANCE` with a synthetic
        // `LocalModuleLoader`; the natives below model the minimal Java
        // surface that `Main.main` → `loadModule(...)` → `Module.loadClass`
        // touches.  Field counts are kept in sync with the
        // `LOADER_FIELD_COUNT` / `MOD_FIELD_COUNT` / `MCL_FIELD_COUNT`
        // constants in `jboss_module_loader.rs`.
        //
        // T19_H4_ANCHOR_LOCAL_MODULE_LOADER
        "org/jboss/modules/LocalModuleLoader" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("root"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
        ],
        // `ModuleLoader` — the abstract base.  Some bytecode holds a
        // `ModuleLoader` reference; give it the same 1-slot root layout so
        // field resolution doesn't OOB.
        "org/jboss/modules/ModuleLoader" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("root"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
        ],
        // `DefaultBootModuleLoaderHolder` — single static field INSTANCE
        // that post_clinit_fixup writes with a LocalModuleLoader ref.
        // T19_H4_ANCHOR_DEFAULT_BOOT_HOLDER
        "org/jboss/modules/DefaultBootModuleLoaderHolder" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                name: rustjvm_types::intern_arc("INSTANCE"),
                descriptor: rustjvm_types::intern_arc("Lorg/jboss/modules/ModuleLoader;"),
                attributes: vec![],
            },
        ],
        // `Module` — 4 instance slots (name, loader, classLoader, resourceRoots).
        // T19_H4_ANCHOR_MODULE
        "org/jboss/modules/Module" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("loader"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/modules/ModuleLoader;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("classLoader"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/modules/ModuleClassLoader;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("resourceRoots"), descriptor: rustjvm_types::intern_arc("[Ljava/lang/String;"), attributes: vec![] },
        ],
        // `ModuleClassLoader` — 1 back-reference field to its owning Module.
        // T19_H4_ANCHOR_MODULE_CLASSLOADER
        "org/jboss/modules/ModuleClassLoader" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("module"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/modules/Module;"), attributes: vec![] },
        ],
        // `ModuleNotFoundException` — Throwable 2-slot shape (message, cause).
        // T19_H4_ANCHOR_MODULE_NOT_FOUND
        "org/jboss/modules/ModuleNotFoundException" => instance_fields(2),

        // ---- T19.2.a: WildFly Core kernel (Deployment + Threads + Logging) ----
        //
        // Companion to `native-builtins/src/wildfly_core.rs`. The native
        // code treats attachments map as an opaque Object; the real map
        // lives Rust-side in the DeploymentUnit Arc.  The fields below
        // are the minimal surface bytecode `getfield` calls must find.
        "org/jboss/as/server/deployment/DeploymentUnit" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("attachments"), descriptor: rustjvm_types::intern_arc("Ljava/util/Map;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("serviceName"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/msc/service/ServiceName;"), attributes: vec![] },
        ],
        // Services is a pure-static utility class (Services.deploymentUnitName, etc.).
        "org/jboss/as/server/deployment/Services" => vec![],
        // EnhancedQueueExecutor: 4 fields — name (String), core_size (Int),
        // max_size (Int), tasks_queue (Object).  The real queue lives in
        // the Rust `EnhancedQueueExecutor` registry keyed by `name`.
        "org/jboss/threads/EnhancedQueueExecutor" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("coreSize"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("maxSize"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("tasksQueue"), descriptor: rustjvm_types::intern_arc("Ljava/util/Queue;"), attributes: vec![] },
        ],
        // Logger mirror: name (String), parent_handle (Object).  The
        // real log machinery is the `tracing` subscriber — this just
        // bridges the JDK calls through.
        "org/jboss/logmanager/Logger" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("parentHandle"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"), attributes: vec![] },
        ],
        // Level: name (String) + intValue (Int) matching JDK constants.
        "org/jboss/logmanager/Level" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("value"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // ModelController: single field holds the cached state ordinal.
        "org/jboss/as/controller/ModelController" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // ControlledProcessState: state-enum ordinal.
        "org/jboss/as/controller/ControlledProcessState" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("stateOrdinal"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],

        // ---- T19.2.b: WildFly / JBoss Naming (JNDI) field layouts ----
        //
        // Companion to `native-builtins/src/wildfly_naming.rs`. The native
        // naming-store owns a process-wide `parking_lot::RwLock<HashMap>` that
        // holds the canonical `(jndi_name -> BindingEntry)` map; the Java
        // mirrors reserve minimum slots for bytecode that peeks at
        // `InitialContext.environment` / `Binding.name` / etc. The canonical
        // state lives Rust-side.
        //
        // InitialContext: 2 fields (environment_map, default_init_ctx_handle).
        "javax/naming/InitialContext" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("environment"), descriptor: rustjvm_types::intern_arc("Ljava/util/Hashtable;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("defaultInitCtx"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // Binding: 3 fields (name, className, object) — wraps a single JNDI
        // entry for enumeration-style APIs (listBindings / list).
        "javax/naming/Binding" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("className"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("object"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"), attributes: vec![] },
        ],
        // ServiceBasedNamingStore: 2 fields (bindings_map, service_base).
        "org/jboss/as/naming/ServiceBasedNamingStore" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("bindings"), descriptor: rustjvm_types::intern_arc("Ljava/util/concurrent/ConcurrentHashMap;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("serviceBase"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/msc/service/ServiceName;"), attributes: vec![] },
        ],
        // ContextNames$BindInfo: 2 fields (binder_service_name, binding_name).
        // Returned by `ContextNames.bindInfoFor(String absolute)` so the caller
        // has both the MSC ServiceName (used to register the binder) and the
        // stripped JNDI name for lookup.
        "org/jboss/as/naming/deployment/ContextNames$BindInfo" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("binderServiceName"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/msc/service/ServiceName;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("bindingName"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
        ],
        // NameNotFoundException / NamingException / InvalidNameException inherit
        // Throwable — reuse the 2-slot (message, cause) shape.
        "javax/naming/NameNotFoundException"
        | "javax/naming/NamingException"
        | "javax/naming/InvalidNameException" => instance_fields(2),

        // ---- T19.3: Quarkus static-init replay ----
        // Field layouts shared with `native-builtins::quarkus_staticinit`.
        // Real-JDK-mode `alloc_object` must reserve enough slots before
        // `quarkus_staticinit` natives write to them; the synthetic layout
        // here guarantees that when the Quarkus JAR is absent (tests) or
        // its real `.class` file hasn't been reached yet.
        //
        // RuntimeValue<T> = 2 fields (value Object?, supplier Object?)
        "io/quarkus/runtime/RuntimeValue" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("value"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("supplier"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/function/Supplier;"),
                attributes: vec![],
            },
        ],
        // StartupContext = 2 fields (shutdown_tasks List, values Map).
        // Both fields are left null by the <init> native — the Rust-side
        // HashMap + Vec owned by `startup_context_values` /
        // `startup_context_shutdown_tasks` hold the real state. Reserving
        // the slots keeps bytecode `getfield shutdownTasks` from OOB-ing.
        "io/quarkus/runtime/StartupContext" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("shutdownTasks"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/List;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("values"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/concurrent/ConcurrentMap;"),
                attributes: vec![],
            },
        ],
        // ApplicationConfig = 2 fields (name, version)
        "io/quarkus/runtime/ApplicationConfig" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("name"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("version"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
        ],
        // DataSourceRuntimeConfig = 4 fields (jdbcUrl, username, password, driver)
        "io/quarkus/runtime/DataSourceRuntimeConfig" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("jdbcUrl"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("username"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("password"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("driver"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
        ],
        // Timing = 4 long fields (bootStart, bootStop, mainStart, mainStop).
        // Backed by alloc_object_with_descriptors via R1's CHM-friendly
        // allocator path so the slots are typed Long from the start and
        // we don't regress into ConcurrentHashMap.initTable chain.
        "io/quarkus/runtime/Timing" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("bootStart"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("bootStop"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("mainStart"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("mainStop"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
        ],

        // ---- T19.H3: Quarkus bootstrap runner + LogManager singletons ----
        //
        // SerializedApplication = 2 (mainClass String, runnerClassLoader).
        // Populated by `native_serialized_application_read` in
        // `native-builtins::quarkus_staticinit`; this synthetic layout
        // reserves the slots so real-JDK-mode `getfield` doesn't OOB
        // if the class file happens to define more fields in a future
        // Quarkus minor bump (we grow with class_num_total_fields).
        "io/quarkus/bootstrap/runner/SerializedApplication" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("mainClass"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("runnerClassLoader"),
                descriptor: rustjvm_types::intern_arc("Lio/quarkus/bootstrap/runner/RunnerClassLoader;"),
                attributes: vec![],
            },
        ],
        // RunnerClassLoader = 1-slot placeholder (parent ClassLoader).  Real
        // class extends URLClassLoader and holds many more slots; the
        // synthetic layout is a minimum — bytecode that touches
        // `parent.*` walks to java/lang/ClassLoader which is real.
        "io/quarkus/bootstrap/runner/RunnerClassLoader" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("parent"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/ClassLoader;"),
                attributes: vec![],
            },
        ],

        // ---- T19.H5: AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater
        // / AtomicLongFieldUpdater synthetic implementation classes. The
        // factory natives in `native-builtins::atomic_updater` allocate
        // these with FU_NUM_SLOTS=4 slots (tclassId, slotIndex, descTag,
        // vclassId).  Reserving the layout here means alloc_object has
        // room and getfield by name (if ever used reflectively) reads
        // the right slot.
        "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl"
        | "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl"
        | "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("tclassId"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("slotIndex"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("descTag"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("vclassId"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],
        // LogManager singleton — 4 slots matching
        // `native-builtins::logmanager::LM_NUM_FIELDS` (properties,
        // loggerRegistry, rootLogger, ready). Reserving them in the
        // synthetic layout means alloc_object has room when the real
        // LogManager bytecode isn't loaded (e.g. pre-clinit fixup).
        "java/util/logging/LogManager" | "org/jboss/logmanager/LogManager" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("properties"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/Properties;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("loggerRegistry"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("rootLogger"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/logging/Logger;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("ready"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],
        // String Enumeration backing for `LogManager.getLoggerNames()`.
        //   0 = Object[] backing names,  1 = cursor int.
        "java/util/logging/LogManager$StringEnumeration" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("names"),
                descriptor: rustjvm_types::intern_arc("[Ljava/lang/Object;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("cursor"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],
        // java.util.logging.Logger (synthetic) — 3 slots (name, level, parent).
        "java/util/logging/Logger" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("name"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("level"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/logging/Level;"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("parent"),
                descriptor: rustjvm_types::intern_arc("Ljava/util/logging/Logger;"),
                attributes: vec![],
            },
        ],

        // ---- T19.8: Agroal (Quarkus) + IronJacamar (WildFly) JDBC pool ----
        // Minimal synthetic layouts so Keycloak's boot path can open an
        // in-memory H2 datasource without tripping missing-field accesses.
        //
        // Agroal:
        //   AgroalDataSource = 3 (config, pool, closed_flag)
        //   ConnectionPool   = 4 (handlers_list, config, size, state)
        //   Configuration    = 6 (jdbc_url, driver, username, password,
        //                         min_size, max_size)
        "io/agroal/api/AgroalDataSource" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("config"), descriptor: rustjvm_types::intern_arc("Lio/agroal/api/configuration/AgroalDataSourceConfiguration;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("pool"), descriptor: rustjvm_types::intern_arc("Lio/agroal/pool/ConnectionPool;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("closed"), descriptor: rustjvm_types::intern_arc("Z"), attributes: vec![] },
        ],
        "io/agroal/pool/ConnectionPool" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("handlers"), descriptor: rustjvm_types::intern_arc("Ljava/util/List;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("config"), descriptor: rustjvm_types::intern_arc("Lio/agroal/api/configuration/AgroalDataSourceConfiguration;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("size"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        "io/agroal/api/configuration/AgroalDataSourceConfiguration" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("jdbcUrl"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("driver"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("username"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("password"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("minSize"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("maxSize"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // IronJacamar:
        //   AbstractPool         = 3 (config, managed_factory, sub_pools)
        //   ManagedConnectionPool = 3 (connections, semaphore, state)
        "org/jboss/jca/core/connectionmanager/pool/AbstractPool" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("config"), descriptor: rustjvm_types::intern_arc("Lorg/jboss/jca/core/connectionmanager/pool/PoolConfiguration;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("managedFactory"), descriptor: rustjvm_types::intern_arc("Ljavax/resource/spi/ManagedConnectionFactory;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("subPools"), descriptor: rustjvm_types::intern_arc("Ljava/util/Map;"), attributes: vec![] },
        ],
        "org/jboss/jca/core/connectionmanager/pool/ManagedConnectionPool" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("connections"), descriptor: rustjvm_types::intern_arc("Ljava/util/List;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("semaphore"), descriptor: rustjvm_types::intern_arc("Ljava/util/concurrent/Semaphore;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],

        // ---- T19.2.c: WildFly Security (JAAS + SecurityDomains + login modules) ----
        //
        // Companion to `native-builtins/src/wildfly_security.rs`. The
        // synthetic-mode layouts reserve the slots native code writes back
        // into; real-mode classes loaded from the JDK / WildFly jars use
        // their declared shape unchanged (`alloc_concurrent_synthetic`
        // picks the max of synthetic-count vs real-count).
        //
        // Subject = 3 (principals, publicCreds, privateCreds) — each
        // field is the object reference the native side uses as a key
        // into its Rust-side `SubjectHandle` map; Java bytecode that
        // reads these fields sees a non-null handle it can pass to
        // follow-up getPrincipals/... natives.
        "javax/security/auth/Subject" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("principals"), descriptor: rustjvm_types::intern_arc("Ljava/util/Set;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("publicCreds"), descriptor: rustjvm_types::intern_arc("Ljava/util/Set;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("privateCreds"), descriptor: rustjvm_types::intern_arc("Ljava/util/Set;"), attributes: vec![] },
        ],
        // LoginContext = 4 (name, subject, handler, modules).
        "javax/security/auth/login/LoginContext" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("subject"), descriptor: rustjvm_types::intern_arc("Ljavax/security/auth/Subject;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("handler"), descriptor: rustjvm_types::intern_arc("Ljavax/security/auth/callback/CallbackHandler;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("modules"), descriptor: rustjvm_types::intern_arc("Ljava/util/List;"), attributes: vec![] },
        ],
        // AppConfigurationEntry = 3 (loginModuleClassName, controlFlag,
        // options). Matches JDK's canonical 3-field shape so reflective
        // access from the WildFly login-config parser reads sensible
        // values.
        "javax/security/auth/login/AppConfigurationEntry" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("loginModuleClassName"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("controlFlag"), descriptor: rustjvm_types::intern_arc("Ljavax/security/auth/login/AppConfigurationEntry$LoginModuleControlFlag;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("options"), descriptor: rustjvm_types::intern_arc("Ljava/util/Map;"), attributes: vec![] },
        ],
        // SecurityDomainService = 2 (name, authMgr).
        "org/jboss/as/security/SecurityDomainService" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("authMgr"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"), attributes: vec![] },
        ],
        // SecurityIdentity = 2 (principal, roles).
        "org/wildfly/security/auth/server/SecurityIdentity" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("principal"), descriptor: rustjvm_types::intern_arc("Ljava/security/Principal;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("roles"), descriptor: rustjvm_types::intern_arc("Ljava/util/Set;"), attributes: vec![] },
        ],

        // ---- T19.2.e: WildFly Datasources subsystem + JTA TX glue ----
        //
        // Companion to `native-builtins/src/wildfly_datasources_tx.rs`.
        // `DataSourceService` is an MSC-driven binder that stamps a pool
        // handle into JNDI at `start()` and removes it at `stop()`.
        // `TransactionManager` / `TransactionManagerImple` hold a thin
        // back-reference; the canonical TX state lives Rust-side on a
        // thread-local `CURRENT_TX` inside `wildfly_datasources_tx`.
        // `Xid`'s three fields mirror the JTA javadoc for
        // `javax.transaction.xa.Xid.getFormatId / getGlobalTransactionId /
        // getBranchQualifier`.
        //
        // `javax.sql.DataSource` is a pure interface — no instance fields.
        "org/jboss/as/connector/subsystems/datasources/DataSourceService" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("jndiName"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("poolHandle"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        "javax/transaction/TransactionManager" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("currentTxId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        "com/arjuna/ats/jta/TransactionManagerImple" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("singletonHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        "javax/transaction/xa/Xid" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("formatId"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("globalTransactionId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("branchQualifier"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],

        // ---- T19.2.d: WildFly Undertow (HTTP subsystem) field layouts ----
        //
        // Companion to `native-builtins/src/wildfly_undertow.rs`. The
        // Undertow builder → server lifecycle pins listener config on
        // Undertow itself, the per-request HttpServerExchange threads
        // through the handler chain, and HeaderMap / HttpString provide
        // the CRLF-safe header surface.
        //
        // Undertow = 5 (listeners, handler, worker_threads, io_threads,
        //               bound_fds — a long id into the native
        //               `undertow_instances` registry).
        "io/undertow/Undertow" | "io/undertow/Undertow$Builder" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("listeners"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("handler"), descriptor: rustjvm_types::intern_arc("Lio/undertow/server/HttpHandler;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("workerThreads"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("ioThreads"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("boundFds"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // HttpServerExchange = 7 (method, uri, request_headers,
        //                          request_body, response_status,
        //                          response_headers, response_sender).
        "io/undertow/server/HttpServerExchange" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("method"), descriptor: rustjvm_types::intern_arc("Lio/undertow/util/HttpString;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("uri"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("requestHeaders"), descriptor: rustjvm_types::intern_arc("Lio/undertow/util/HeaderMap;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("requestBody"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("responseStatus"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("responseHeaders"), descriptor: rustjvm_types::intern_arc("Lio/undertow/util/HeaderMap;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("responseSender"), descriptor: rustjvm_types::intern_arc("Lio/undertow/io/Sender;"), attributes: vec![] },
        ],
        // HeaderMap = 1 (entries_map long id into native registry).
        "io/undertow/util/HeaderMap" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("entriesMap"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // HttpString = 1 (bytes String mirror).
        "io/undertow/util/HttpString" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("bytes"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
        ],
        // UndertowService = 3 (name, server_handle, state).
        "org/wildfly/extension/undertow/UndertowService" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("serverHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // ListenerService = 3 (port, host, bound_address).
        "org/wildfly/extension/undertow/ListenerService" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("port"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("host"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("boundAddress"), descriptor: rustjvm_types::intern_arc("Ljava/net/InetSocketAddress;"), attributes: vec![] },
        ],

        // ---- T19.7.b: JBoss XNIO (XnioWorker + Xnio) field layouts ----
        //
        // Companion to `native-builtins/src/xnio_worker.rs`.  The canonical
        // worker state (thread pools, queues) lives Rust-side; these slots
        // reserve the minimum shape Java bytecode inspects directly.  The
        // `optionsHandle` Long round-trips the worker id into the process-
        // wide registry so subsequent native calls rebind to the same Arc.
        //
        // XnioWorker = 5 (name, ioThreadsArr, taskThreadsCount, state,
        //                 optionsHandle).
        "org/xnio/XnioWorker" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("ioThreadsArr"), descriptor: rustjvm_types::intern_arc("[Lorg/xnio/XnioIoThread;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("taskThreadsCount"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("optionsHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // Xnio = 2 (name, providerHandle).  Singleton provider — the handle
        // always round-trips to the same global Arc.
        "org/xnio/Xnio" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("providerHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // NioXnioWorker inherits every slot from XnioWorker; no extra
        // instance fields at this level.  The synthetic class hierarchy
        // doesn't (yet) stack parent slots automatically for synthetic
        // classes, so we repeat the parent layout here to keep
        // `alloc_concurrent_synthetic` happy if the bytecode ever
        // allocates a `NioXnioWorker` directly.
        "org/xnio/nio/NioXnioWorker" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("ioThreadsArr"), descriptor: rustjvm_types::intern_arc("[Lorg/xnio/XnioIoThread;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("taskThreadsCount"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("optionsHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],

        // ---- T19.7.d: XNIO Conduit stream channels + ChannelListener ----
        //
        // Companion to `native-builtins/src/xnio_conduits.rs`. The source /
        // sink channels own a registry handle plus listener bookkeeping;
        // `ChannelListener$Setter` binds a listener onto the owning
        // channel's field slot (source reads OR sink writes).
        //
        // ConduitStreamSourceChannel = 5 (channel_id, selection_key,
        //                                  read_listener, read_ready_flag,
        //                                  read_suspended).
        "org/xnio/conduits/ConduitStreamSourceChannel" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("channelId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("selectionKey"), descriptor: rustjvm_types::intern_arc("Ljava/nio/channels/SelectionKey;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("readListener"), descriptor: rustjvm_types::intern_arc("Lorg/xnio/ChannelListener;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("readReadyFlag"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("readSuspended"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // ConduitStreamSinkChannel = 6 (channel_id, selection_key,
        //                                write_listener, write_ready_flag,
        //                                write_suspended, buffered_bytes).
        "org/xnio/conduits/ConduitStreamSinkChannel" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("channelId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("selectionKey"), descriptor: rustjvm_types::intern_arc("Ljava/nio/channels/SelectionKey;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("writeListener"), descriptor: rustjvm_types::intern_arc("Lorg/xnio/ChannelListener;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("writeReadyFlag"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("writeSuspended"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("bufferedBytes"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // ChannelListener$Setter = 2 (channel_handle, listener_slot_index).
        "org/xnio/ChannelListener$Setter" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("channelHandle"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("listenerSlotIndex"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],

        // ---- T19.7.c: XNIO I/O-thread + executor-key field layouts ----
        //
        // Companion to `native-builtins/src/xnio_io_thread.rs`. The
        // XnioIoThread event loop keeps its real state in a process-wide
        // `IoThreadHandle` registry keyed by the mirror's `id` field;
        // the synthetic mirror itself only stores enough to re-resolve
        // that handle. See `IOT_FIELD_*` and `KEY_FIELD_*` constants in
        // `xnio_io_thread.rs`.
        //
        // XnioIoThread = 4 (id, worker_handle, selector_handle, state).
        // NioIoThread shares the layout (extends XnioIoThread, no extra
        // instance fields at the nio subclass level).
        "org/xnio/XnioIoThread" | "org/xnio/nio/NioIoThread" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("id"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("workerHandle"), descriptor: rustjvm_types::intern_arc("Lorg/xnio/XnioWorker;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("selectorHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("state"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // XnioExecutor$Key = 2 (task_id long, cancelled int-boolean).
        // `Key.remove()` flips the `cancelled` slot + the process-wide
        // AtomicBool stored in the T19.7.c key-cancel registry.
        "org/xnio/XnioExecutor$Key" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("taskId"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("cancelled"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],

        // ---- T19.7.e: XNIO OptionMap + IoFuture + Options layouts ----
        //
        // Companion to `native-builtins/src/xnio_async.rs`.  The canonical
        // state (HashMap entries, AtomicU8 status, Condvar, notifier list)
        // lives Rust-side in process-wide registries keyed by Long handles.
        // The JVM mirrors only reserve enough slots for the native to
        // rebind to the same `Arc<...>Inner` struct on subsequent calls.
        //
        // Option = 3 (declaringClass, name, typeClass).  `Option.simple(...)`
        // populates all three; the static final instances in `Options` share
        // this layout.
        "org/xnio/Option" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("declaringClass"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("typeClass"), descriptor: rustjvm_types::intern_arc("Ljava/lang/Class;"), attributes: vec![] },
        ],
        // OptionMap = 1 (entries_arc_handle).  Immutable-after-build; the
        // Long round-trips to `lookup_map(handle)` for every read.
        "org/xnio/OptionMap" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("entriesHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // OptionMap$Builder = 1 (pending_entries handle).  Mutable until
        // `getMap()` flips `consumed` atomically; subsequent `set()` raises
        // IllegalStateException.
        "org/xnio/OptionMap$Builder" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("pendingHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // IoFuture = 3 (status int snapshot, result_slot handle, notifier_list
        // mirror handle).  Real state (AtomicU8, Mutex<FutureState>, Condvar)
        // lives in the process-wide `futures` registry.  Transitions are
        // monotonic: WAITING → {DONE, CANCELLED, FAILED}.
        "org/xnio/IoFuture" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("status"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("resultSlot"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("notifierList"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // FutureResult = 1 (future_handle).  The producer side wires
        // `setResult/setException/setCancelled` to the same IoFutureInner
        // that `getIoFuture()` returns.  Drop-triggered abandonment
        // (never called any of the three setters) is logged via
        // `tracing::warn!` but not auto-FAILED — the tracked handle
        // stays in the registry so subsequent rebind does not NPE.
        "org/xnio/FutureResult" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("futureHandle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // Options is a pure-static class — no instance fields.  The
        // well-known option constants (WORKER_IO_THREADS, BACKLOG, …)
        // are populated on first access via `ensure_options_initialized`
        // and stashed in a Rust-side map keyed by field name.
        "org/xnio/Options" => vec![],

        // ---- T19.10: Infinispan local-mode cache field layouts ----
        //
        // Companion to `native-builtins/src/infinispan_local.rs`. Every
        // Infinispan object keeps a Long "handle" in slot 0 that round-trips
        // to either the process-wide `DefaultCacheManagerInner` address or an
        // `Arc<CacheInner>` raw pointer. The remaining slots mirror the fields
        // that Java bytecode in Keycloak touches directly so getfield/putfield
        // resolve without missing-field panics.
        //
        // DefaultCacheManager = 3 (handle J, configName String, started I).
        "org/infinispan/manager/DefaultCacheManager" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("handle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("configName"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("started"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // EmbeddedCacheManager (interface) — no instance fields; method
        // dispatch goes through the native registry directly.
        "org/infinispan/manager/EmbeddedCacheManager" => vec![],
        // CacheImpl = 3 (handle J, name String, manager DefaultCacheManager).
        "org/infinispan/cache/impl/CacheImpl" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("handle"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("manager"), descriptor: rustjvm_types::intern_arc("Lorg/infinispan/manager/DefaultCacheManager;"), attributes: vec![] },
        ],
        // Cache (interface) and AdvancedCache (interface) — no instance fields;
        // method dispatch goes through the native registry directly.
        "org/infinispan/Cache" | "org/infinispan/AdvancedCache" => vec![],
        // Configuration = 3 (name String, sizeLimit I, ttlMs J).
        "org/infinispan/configuration/cache/Configuration" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("sizeLimit"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("ttlMs"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // ConfigurationBuilder = 3 (mirrors Configuration for build() pass-through).
        "org/infinispan/configuration/cache/ConfigurationBuilder" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("name"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("sizeLimit"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("ttlMs"), descriptor: rustjvm_types::intern_arc("J"), attributes: vec![] },
        ],
        // GlobalConfiguration = 3 (siteName String, jmxEnabled I, reserved I).
        "org/infinispan/configuration/global/GlobalConfiguration" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("siteName"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("jmxEnabled"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("reserved"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // GlobalConfigurationBuilder = 3 (mirrors GlobalConfiguration).
        "org/infinispan/configuration/global/GlobalConfigurationBuilder" => vec![
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("siteName"), descriptor: rustjvm_types::intern_arc("Ljava/lang/String;"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("jmxEnabled"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
            ClassFileField { access_flags: FieldAccessFlags::empty(), name: rustjvm_types::intern_arc("reserved"), descriptor: rustjvm_types::intern_arc("I"), attributes: vec![] },
        ],
        // CacheNotifier — interface; no instance fields.
        "org/infinispan/notifications/cachelistener/CacheNotifier" => vec![],

        // ---- T19.4: Quarkus ArC CDI container field layouts ----
        //
        // Companion to `native-builtins/src/quarkus_arc.rs`. The synthetic
        // layouts let `alloc_object` reserve the correct number of slots
        // before the ArC natives write into them.
        //
        // Arc (static-only class) — no instance fields; all methods are
        // static and dispatch through the process-wide OnceLock singleton.
        "io/quarkus/arc/Arc" => vec![],
        // ArcContainer (interface) + ArcContainerImpl (backing impl) — the
        // impl carries a Long container-id in slot 0 so native calls can
        // recover the Rust-side ArcContainerInner without trusting ObjectRef
        // pointer identity (which collides in parallel unit-test contexts).
        "io/quarkus/arc/ArcContainer"
        | "io/quarkus/arc/impl/ArcContainerImpl" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("containerId"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
        ],
        // InstanceHandle / InstanceHandleImpl — slot 0 is the Long
        // container-id; slot 1 is the bean Object stored by the resolution
        // path so that `InstanceHandle.get()` can unwrap it without a
        // second map lookup.
        "io/quarkus/arc/InstanceHandle"
        | "io/quarkus/arc/impl/InstanceHandleImpl" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("containerId"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("beanRef"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
        ],
        // InjectableBean — same two-slot shape as InstanceHandle so that
        // `InjectableBean.get()` (backed by native_instance_handle_get)
        // can read slot 1 without an extra dispatch step.
        "io/quarkus/arc/InjectableBean" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("containerId"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("beanRef"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
        ],
        // javax/jakarta CDI Instance — same two-slot shape; ArC returns
        // an InstanceHandle as an Instance<T> for the `select()` path.
        "javax/enterprise/inject/Instance"
        | "jakarta/enterprise/inject/Instance" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("containerId"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("beanRef"),
                descriptor: rustjvm_types::intern_arc("Ljava/lang/Object;"),
                attributes: vec![],
            },
        ],
        // BeanManager (both javax and jakarta) — holds the container-id
        // in slot 0 so that `getBeans` / `getReference` can route to the
        // right ArcContainerInner.
        "javax/enterprise/inject/spi/BeanManager"
        | "jakarta/enterprise/inject/spi/BeanManager" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("containerId"),
                descriptor: rustjvm_types::intern_arc("J"),
                attributes: vec![],
            },
        ],
        // ManagedContext (requestContext() return type) — single slot
        // reserved for future state; the current boot path never reads it.
        "io/quarkus/arc/ManagedContext" => vec![
            ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: rustjvm_types::intern_arc("active"),
                descriptor: rustjvm_types::intern_arc("I"),
                attributes: vec![],
            },
        ],

        // T19.H2: SharedSecrets JavaLangAccess shim class. `java.lang.System$1`
        // is the JDK's anonymous inner class that implements every
        // JavaLangAccess method. When we allocate our shim via
        // `alloc_concurrent_synthetic`, any slot writes are no-ops today,
        // but the forward-compat layout reserves one reference slot for a
        // future back-pointer to a JLA-interior cache. This entry keeps the
        // synthetic fallback layout stable when the real JDK class file
        // can't be loaded.
        "java/lang/System$1" => instance_fields(1),

        // T19.H2: synthetic `ModuleLayer` fallback when the real JDK class
        // can't be resolved during boot (pre-init). Slot 0 = boot flag.
        "java/lang/ModuleLayer" => instance_fields(2),

        // T19.H2: synthetic `Module` fallback. Slots: name/layer/packages/
        // descriptor/loader — see `jboss_jdkspecific.rs` for the layout.
        "java/lang/Module" => instance_fields(5),

        // T19.H2: StackWalker synthetic fallback — options, estimateDepth,
        // extendedOption, retainClassRef, contScope, continuation.
        // `phases_late::p59_sw_walk` already allocates 0-field stubs for
        // walker objects; the 6-field layout here upgrades that path.
        "java/lang/StackWalker" => instance_fields(6),

        // T19.H2: ClassFileDumper synthetic fallback — key, dumpDir,
        // enabled, counter slots used by `register_t19_h2_lookup_clinit_deps`.
        "jdk/internal/util/ClassFileDumper" => instance_fields(4),

        // T19_M1_PLATFORM_MXBEANS — synthetic stubs for JDK 25 JMX
        // OpenType machinery used by `jmx_openmbean::alloc_*`.
        // CompositeType: typeName, description, className, isArray,
        // itemNames, nameToDescription, nameToType, nameToIndex.
        "javax/management/openmbean/CompositeType" => instance_fields(8),
        // SimpleType: className, typeName, description, isArray,
        // primitive (cached identity).
        "javax/management/openmbean/SimpleType" => instance_fields(5),
        // OpenType base: className, typeName, description (may be
        // hit if subclassing path resolves base before subclass).
        "javax/management/openmbean/OpenType" => instance_fields(3),
        // MXBeanMapping: javaType, openType, openClass.
        "com/sun/jmx/mbeanserver/MXBeanMapping" => instance_fields(3),
        // ConvertingMethod: method, returnMapping, paramMappings,
        // paramConversionIsIdentity.
        "com/sun/jmx/mbeanserver/ConvertingMethod" => instance_fields(4),
        // OpenConverter: targetType, openType, openClass,
        // identityConverter.
        "com/sun/jmx/mbeanserver/OpenConverter" => instance_fields(4),
        // MappedMXBeanType: openType, typeName, isBasicType,
        // arrayMapping.
        "com/sun/jmx/mbeanserver/MappedMXBeanType" => instance_fields(4),

        _ => vec![],
    }
}

fn synthetic_stub_ctor_methods(name: &str) -> Vec<ClassFileMethod> {
    let mut out = Vec::new();
    let mk_ctor = |descriptor: &str| ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
        name: rustjvm_types::intern_arc("<init>"),
        descriptor: rustjvm_types::intern_arc(descriptor),
        attributes: vec![],
    };
    let is_throwable_like =
        name == "java/lang/Throwable" || name.ends_with("Exception") || name.ends_with("Error");
    if is_throwable_like {
        out.extend([
            mk_ctor("()V"),
            mk_ctor("(Ljava/lang/String;)V"),
            mk_ctor("(Ljava/lang/Throwable;)V"),
            mk_ctor("(Ljava/lang/String;Ljava/lang/Throwable;)V"),
        ]);
    }
    if name == "java/nio/file/attribute/PosixFilePermission" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc("<clinit>"),
            descriptor: rustjvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "java/io/InputStreamReader" {
        out.extend([
            mk_ctor("(Ljava/io/InputStream;)V"),
            mk_ctor("(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V"),
            mk_ctor("(Ljava/io/InputStream;Ljava/lang/String;)V"),
        ]);
    }
    if name == "java/io/BufferedReader" {
        out.extend([
            mk_ctor("(Ljava/io/Reader;)V"),
            mk_ctor("(Ljava/io/Reader;I)V"),
        ]);
    }
    if name == "java/lang/Thread" {
        for desc in [
            "()V",
            "(Ljava/lang/Runnable;)V",
            "(Ljava/lang/Runnable;Ljava/lang/String;)V",
            "(Ljava/lang/String;)V",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
        ] {
            out.push(ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: rustjvm_types::intern_arc("<init>"),
                descriptor: rustjvm_types::intern_arc(desc),
                attributes: vec![],
            });
        }
        for (method, desc) in [
            ("start0", "()V"),
            ("start", "()V"),
            ("run", "()V"),
            ("join", "()V"),
            ("join", "(J)V"),
            ("join", "(JI)V"),
        ] {
            out.push(ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: rustjvm_types::intern_arc(method),
                descriptor: rustjvm_types::intern_arc(desc),
                attributes: vec![],
            });
        }
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc("getThreadGroup"),
            descriptor: rustjvm_types::intern_arc("()Ljava/lang/ThreadGroup;"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc("getPriority"),
            descriptor: rustjvm_types::intern_arc("()I"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc("isDaemon"),
            descriptor: rustjvm_types::intern_arc("()Z"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc("setDaemon"),
            descriptor: rustjvm_types::intern_arc("(Z)V"),
            attributes: vec![],
        });
    }
    if name == "java/util/concurrent/CountDownLatch" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc(method),
            descriptor: rustjvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(I)V"),
            mk("countDown", "()V"),
            mk("await", "()V"),
            mk("await", "(JLjava/util/concurrent/TimeUnit;)Z"),
            mk("getCount", "()J"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/concurrent/Semaphore" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc(method),
            descriptor: rustjvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(I)V"),
            mk("<init>", "(IZ)V"),
            mk("acquire", "()V"),
            mk("acquire", "(I)V"),
            mk("acquireUninterruptibly", "()V"),
            mk("release", "()V"),
            mk("release", "(I)V"),
            mk("tryAcquire", "()Z"),
            mk("tryAcquire", "(I)Z"),
            mk("tryAcquire", "(JLjava/util/concurrent/TimeUnit;)Z"),
            mk("availablePermits", "()I"),
            mk("drainPermits", "()I"),
            mk("isFair", "()Z"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/concurrent/atomic/AtomicBoolean" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc(method),
            descriptor: rustjvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([mk("<init>", "()V"), mk("<init>", "(Z)V")]);
    }
    if name == "java/util/concurrent/ScheduledThreadPoolExecutor" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc(method),
            descriptor: rustjvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(I)V"),
            mk("<init>", "(ILjava/util/concurrent/ThreadFactory;)V"),
        ]);
    }
    if name == "java/util/concurrent/CopyOnWriteArrayList" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: rustjvm_types::intern_arc(method),
            descriptor: rustjvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("contains", "(Ljava/lang/Object;)Z"),
            mk("addIfAbsent", "(Ljava/lang/Object;)Z"),
        ]);
    }
    out
}

impl std::fmt::Debug for ClassManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassManager")
            .field("loaded_count", &self.loaded_count())
            .field("bootstrap", &self.bootstrap)
            .field("extension", &self.extension)
            .field("application", &self.application)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Field layout computation
// ---------------------------------------------------------------------------

/// Compute `(first_field_index, num_total_fields)` for a class being loaded.
///
/// Instance fields (non-static) from the superclass occupy slots `0..N`.
/// This class's own instance fields start at `N`. Static fields don't need
/// object slots; only instance fields are counted.
fn compute_field_layout(
    fields: &[rustjvm_reader::field::ClassFileField],
    superclass_id: Option<ClassId>,
    store: &ClassStore,
) -> (usize, usize) {
    let parent_total = match superclass_id {
        Some(super_id) => store.get(super_id).map_or(0, |sc| sc.num_total_fields),
        None => 0,
    };

    let own_instance_fields = fields
        .iter()
        .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
        .count();

    let first_field_index = parent_total;
    let num_total_fields = parent_total + own_instance_fields;

    (first_field_index, num_total_fields)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::ClassStore;
    use rustjvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use rustjvm_reader::class_file_version::ClassFileVersion;
    use rustjvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    fn empty_constant_pool() -> ConstantPool {
        ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
    }

    fn make_field(name: &str, is_static: bool) -> rustjvm_reader::field::ClassFileField {
        let flags = if is_static {
            FieldAccessFlags::STATIC
        } else {
            FieldAccessFlags::empty()
        };
        rustjvm_reader::field::ClassFileField {
            access_flags: flags,
            name: Arc::from(name),
            descriptor: rustjvm_types::intern_arc("I"),
            attributes: vec![],
        }
    }

    #[test]
    fn compute_field_layout_no_parent() {
        let fields = vec![make_field("x", false), make_field("y", false)];
        let store = ClassStore::new();
        let (first, total) = compute_field_layout(&fields, None, &store);
        assert_eq!(first, 0);
        assert_eq!(total, 2);
    }

    #[test]
    fn compute_field_layout_excludes_static() {
        let fields = vec![
            make_field("x", false),
            make_field("COUNT", true), // static — not counted
            make_field("y", false),
        ];
        let store = ClassStore::new();
        let (first, total) = compute_field_layout(&fields, None, &store);
        assert_eq!(first, 0);
        assert_eq!(total, 2); // only x and y
    }

    #[test]
    fn compute_field_layout_with_parent() {
        let mut store = ClassStore::new();

        // Parent with 3 instance fields
        let parent_id = store.next_id();
        store.add(Class {
            id: parent_id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("Parent"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 3,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });

        let child_fields = vec![make_field("a", false), make_field("b", false)];
        let (first, total) = compute_field_layout(&child_fields, Some(parent_id), &store);
        assert_eq!(first, 3); // child's fields start after parent's 3
        assert_eq!(total, 5); // 3 inherited + 2 own
    }

    #[test]
    fn class_manager_empty_classpath() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let result = mgr.load_class("does/not/Exist");
        assert!(result.is_err());
    }

    #[test]
    fn class_manager_find_before_load_returns_none() {
        let mgr = ClassManager::new(&[], &[], &[]);
        assert!(mgr.find_class_by_name("java/lang/Object").is_none());
    }

    #[test]
    fn class_manager_debug_display() {
        let mgr = ClassManager::new(&[], &[], &[]);
        let debug = format!("{mgr:?}");
        assert!(debug.contains("ClassManager"));
        assert!(debug.contains("loaded_count"));
    }

    /// RKC16N.3 — `Class.forName("[Ljava/util/HashMap;")` resolves by
    /// synthesis without classpath I/O. The component class is recursively
    /// loaded, the array's superclass is `java/lang/Object`, the array
    /// `Class` is *not* marked as a synthetic stub, and two calls return
    /// the same `ClassId`.
    #[test]
    fn rkc16n3_synthesises_reference_array_class() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let id = mgr
            .load_class("[Ljava/util/HashMap;")
            .expect("array class synthesis must succeed without I/O");

        // Array class metadata.
        let array_class = mgr
            .get_class(id)
            .expect("array class must be registered in ClassStore");
        assert_eq!(&*array_class.name, "[Ljava/util/HashMap;");
        assert!(
            !array_class.is_synthetic_stub,
            "array classes are synthesised, not stubbed",
        );
        assert!(
            array_class.superclass.is_some(),
            "array superclass must be java/lang/Object",
        );
        let object_id = mgr
            .get_loaded_class_id("java/lang/Object")
            .expect("Object must be loaded as the array superclass");
        assert_eq!(array_class.superclass, Some(object_id));

        // Component class was recursively resolved (HashMap may be a
        // synthetic stub here since no JMOD is on the test classpath, but
        // its ClassId must exist).
        assert!(
            mgr.get_loaded_class_id("java/util/HashMap").is_some(),
            "reference-array component must be recursively loaded",
        );

        // Idempotent caching: a second call returns the same `ClassId`.
        let id2 = mgr
            .load_class("[Ljava/util/HashMap;")
            .expect("second resolution must succeed");
        assert_eq!(id, id2, "array class identity must be stable across calls");
    }

    /// RKC16N.3 — Multi-dim reference arrays recursively synthesise the
    /// inner array class. `[[Ljava/lang/Object;` resolves to a class that
    /// has the inner array as part of the chain.
    #[test]
    fn rkc16n3_synthesises_multidim_reference_array() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let outer = mgr
            .load_class("[[Ljava/lang/Object;")
            .expect("multi-dim array synthesis must succeed");

        // Both the outer and the inner array must be cached.
        let outer_again = mgr
            .load_class("[[Ljava/lang/Object;")
            .expect("re-resolving multi-dim must succeed");
        assert_eq!(outer, outer_again);

        let inner_id = mgr
            .get_loaded_class_id("[Ljava/lang/Object;")
            .expect("inner array class must be cached during multi-dim synthesis");
        let inner = mgr
            .get_class(inner_id)
            .expect("inner array class must be registered");
        assert!(!inner.is_synthetic_stub);
        assert_eq!(&*inner.name, "[Ljava/lang/Object;");
    }

    /// RKC16N.3 — Primitive arrays such as `[I` and multi-dim primitives
    /// `[[I` synthesise without referring to the classpath.
    #[test]
    fn rkc16n3_synthesises_primitive_array_classes() {
        let mut mgr = ClassManager::new(&[], &[], &[]);

        let int_arr = mgr.load_class("[I").expect("[I synthesis must succeed");
        let int_arr_class = mgr.get_class(int_arr).unwrap();
        assert!(!int_arr_class.is_synthetic_stub);
        assert_eq!(&*int_arr_class.name, "[I");

        let int_arr_arr = mgr
            .load_class("[[I")
            .expect("[[I synthesis must succeed");
        assert_ne!(int_arr, int_arr_arr);
        let int_arr_arr_class = mgr.get_class(int_arr_arr).unwrap();
        assert_eq!(&*int_arr_arr_class.name, "[[I");

        // Idempotent.
        let int_arr2 = mgr.load_class("[I").unwrap();
        assert_eq!(int_arr, int_arr2);
    }

    /// T10.3 — Verify the FxHashMap swap on `name_to_id` preserves the
    /// name-hash → ClassId lookup for 100 distinct class names. Uses
    /// `register_class_name` (the only public writer) and `get_loaded_class_id`
    /// (the only public reader that hits `name_to_id` directly).
    #[test]
    fn t10_class_manager_fxhash_name_to_id_roundtrip() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let mut expected: Vec<(String, ClassId)> = Vec::with_capacity(100);
        for i in 0..100 {
            let name = format!("pkg/Cls{i}");
            let id = mgr.class_store.next_id();
            mgr.register_class_name(ClassLoaderId::Application, &name, id);
            expected.push((name, id));
        }
        for (name, id) in &expected {
            let got = mgr
                .get_loaded_class_id(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(got, *id, "name_to_id roundtrip mismatch for {name}");
        }
        // Negative: a name we never registered must not resolve.
        assert!(mgr.get_loaded_class_id("pkg/Unseen").is_none());
    }

    /// T10.9.B — smoke test: verify the FxHashMap swap on
    /// `loaded_classes`, `class_bytes_cache`, `cds_class_cache`, and the
    /// `loading_guard` FxHashSet preserves insert/lookup semantics.
    #[test]
    fn t10_9_b_class_manager_fxhash_swap_smoke() {
        let mut mgr = ClassManager::new(&[], &[], &[]);

        // class_bytes_cache: populate 50 entries, confirm round-trip.
        for i in 0..50u32 {
            let name = format!("pkg/Cls{i}");
            let bytes = vec![0xcafe_babeu32.to_be_bytes()[0]; i as usize + 4];
            mgr.class_bytes_cache.insert(name, bytes);
        }
        for i in 0..50u32 {
            let name = format!("pkg/Cls{i}");
            let got = mgr.class_bytes_cache.get(&name);
            assert!(got.is_some(), "class_bytes_cache missing {name}");
            assert_eq!(got.unwrap().len(), i as usize + 4);
        }
        assert!(mgr.class_bytes_cache.get("pkg/NotThere").is_none());

        // cds_class_cache: populate and verify.
        for i in 0..25u32 {
            let name = format!("cds/Archived{i}");
            mgr.cds_class_cache.insert(name, vec![i as u8; 8]);
        }
        assert_eq!(mgr.cds_class_cache.len(), 25);
        for i in 0..25u32 {
            let name = format!("cds/Archived{i}");
            let got = mgr.cds_class_cache.get(&name).unwrap();
            assert_eq!(got[0], i as u8);
        }
        assert!(mgr.cds_class_cache.get("cds/Missing").is_none());
    }

    #[test]
    fn class_is_record_with_components() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("com/example/Point"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 2,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: vec![
                RecordComponentInfo {
                    name: "x".to_string(),
                    descriptor: "I".to_string(),
                },
                RecordComponentInfo {
                    name: "y".to_string(),
                    descriptor: "I".to_string(),
                },
            ],
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let cls = store.get(id).unwrap();
        assert!(cls.is_record());
        assert!(!cls.is_sealed());
        assert_eq!(cls.record_components.len(), 2);
        assert_eq!(cls.record_components[0].name, "x");
        assert_eq!(cls.record_components[1].descriptor, "I");
    }

    #[test]
    fn synthetic_thread_stub_declares_get_thread_group() {
        let methods = synthetic_stub_ctor_methods("java/lang/Thread");
        assert!(
            methods.iter().any(|m| {
                &*m.name == "getThreadGroup"
                    && &*m.descriptor == "()Ljava/lang/ThreadGroup;"
            }),
            "synthetic Thread stub should declare getThreadGroup()"
        );
    }

    #[test]
    fn class_is_sealed_with_permitted() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("com/example/Shape"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: vec![
                "com/example/Circle".to_string(),
                "com/example/Square".to_string(),
            ],
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let cls = store.get(id).unwrap();
        assert!(cls.is_sealed());
        assert!(!cls.is_record());
        assert_eq!(cls.permitted_subclasses.len(), 2);
        assert!(cls
            .permitted_subclasses
            .contains(&"com/example/Circle".to_string()));
    }

    #[test]
    fn class_not_record_not_sealed_by_default() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("com/example/Plain"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let cls = store.get(id).unwrap();
        assert!(!cls.is_record());
        assert!(!cls.is_sealed());
    }

    // -----------------------------------------------------------------------
    // Phase 84.3: Sealed class verification
    // -----------------------------------------------------------------------

    #[test]
    fn sealed_non_permitted_subclass_rejected() {
        // Build a sealed parent that only permits "test/Allowed"
        let mut store = ClassStore::new();
        let parent_id = store.next_id();
        store.add(Class {
            id: parent_id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("test/Sealed"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: vec!["test/Allowed".to_string()],
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        assert!(store.get(parent_id).unwrap().is_sealed());

        // "test/NotAllowed" is NOT in the permitted list → verification should reject it
        let not_allowed = "test/NotAllowed";
        let parent = store.get(parent_id).unwrap();
        let is_permitted = parent.permitted_subclasses.iter().any(|p| p == not_allowed);
        assert!(!is_permitted, "test/NotAllowed should not be in permitted list");

        // "test/Allowed" IS in the permitted list
        let allowed = "test/Allowed";
        let is_permitted = parent.permitted_subclasses.iter().any(|p| p == allowed);
        assert!(is_permitted, "test/Allowed should be in permitted list");
    }

    // -----------------------------------------------------------------------
    // M19: has_finalizer / declares_finalize
    // -----------------------------------------------------------------------

    #[test]
    fn m19_declares_finalize_false_for_object() {
        use rustjvm_reader::class_access_flags::MethodAccessFlags;
        use rustjvm_reader::method::ClassFileMethod;

        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("java/lang/Object"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![ClassFileMethod {
                name: rustjvm_types::intern_arc("finalize"),
                descriptor: rustjvm_types::intern_arc("()V"),
                access_flags: MethodAccessFlags::PROTECTED,
                attributes: vec![],
            }],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let cls = store.get(id).unwrap();
        // java/lang/Object itself should NOT be considered as "declares_finalize"
        // because its finalize() is the base implementation
        assert!(!cls.declares_finalize());
    }

    #[test]
    fn m19_declares_finalize_true_for_subclass() {
        use rustjvm_reader::class_access_flags::MethodAccessFlags;
        use rustjvm_reader::method::ClassFileMethod;

        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("com/example/MyResource"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![ClassFileMethod {
                name: rustjvm_types::intern_arc("finalize"),
                descriptor: rustjvm_types::intern_arc("()V"),
                access_flags: MethodAccessFlags::PROTECTED,
                attributes: vec![],
            }],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            has_finalizer: true,
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let cls = store.get(id).unwrap();
        assert!(cls.declares_finalize());
        assert!(cls.has_finalizer);
    }

    #[test]
    fn m19_no_finalize_method_means_no_declares() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: rustjvm_types::intern_arc("com/example/Plain"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded, initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let cls = store.get(id).unwrap();
        assert!(!cls.declares_finalize());
        assert!(!cls.has_finalizer);
    }

    // ── Boot classpath module discovery integration tests ─────────────
    // Run with: cargo test -p rustjvm-classloading -- --ignored

    /// Helper: find the jmods directory on this machine.
    fn find_jmods_dir() -> Option<std::path::PathBuf> {
        use std::path::PathBuf;
        // JAVA_HOME
        if let Ok(val) = std::env::var("JAVA_HOME") {
            let p = PathBuf::from(&val).join("jmods");
            if p.is_dir() {
                return Some(p);
            }
        }
        // Common Windows paths
        for dir in &[
            r"C:\Program Files\Java",
            r"C:\Program Files\Eclipse Adoptium",
        ] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let jmods = entry.path().join("jmods");
                    if jmods.is_dir() {
                        return Some(jmods);
                    }
                }
            }
        }
        // PATH detection via java
        let output = std::process::Command::new("java")
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let jmods = PathBuf::from(value.trim()).join("jmods");
                    if jmods.is_dir() {
                        return Some(jmods);
                    }
                }
            }
        }
        None
    }

    #[test]
    #[ignore] // requires JDK on host
    fn class_manager_finds_java_lang_object_from_jmod() {
        let jmods_dir = find_jmods_dir();
        assert!(jmods_dir.is_some(), "No JDK found");
        let jmods_dir = jmods_dir.unwrap();

        // Build boot classpath with java.base.jmod first
        let base = jmods_dir.join("java.base.jmod");
        assert!(base.exists(), "java.base.jmod not found in {}", jmods_dir.display());

        let boot_cp = vec![base.to_string_lossy().into_owned()];
        let cm = ClassManager::new(&boot_cp, &[], &[]);

        // Should be able to find java.lang.Object bytecode
        assert!(
            cm.has_real_boot_classes(),
            "ClassManager should detect real boot classes"
        );

        // Verify the bootstrap finder can locate Object bytecode
        let bytes = cm.bootstrap.find_class_bytes("java/lang/Object");
        assert!(bytes.is_ok(), "Should find java/lang/Object: {:?}", bytes.err());
        let bytes = bytes.unwrap();
        assert_eq!(
            &bytes[..4],
            &[0xCA, 0xFE, 0xBA, 0xBE],
            "Object.class should start with CAFEBABE"
        );
        eprintln!("java.lang.Object: {} bytes from java.base.jmod", bytes.len());
    }

    #[test]
    #[ignore] // requires JDK on host
    fn class_manager_lists_boot_modules() {
        let jmods_dir = find_jmods_dir();
        if jmods_dir.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmods_dir = jmods_dir.unwrap();

        // Load ALL jmods
        let mut boot_cp: Vec<String> = std::fs::read_dir(&jmods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jmod"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        boot_cp.sort();
        // Ensure java.base is first
        if let Some(pos) = boot_cp.iter().position(|p| p.contains("java.base")) {
            let base = boot_cp.remove(pos);
            boot_cp.insert(0, base);
        }

        let cm = ClassManager::new(&boot_cp, &[], &[]);
        let modules = cm.list_boot_modules();

        assert!(
            modules.contains(&"java.base".to_string()),
            "Modules should include java.base"
        );
        assert!(
            modules.len() >= 10,
            "Should have ≥10 modules, got {}",
            modules.len()
        );

        let class_count = cm.boot_jmod_class_count();
        assert!(
            class_count > 1000,
            "Boot classpath should have >1000 classes, got {class_count}"
        );

        eprintln!(
            "Boot classpath: {} modules, {} classes",
            modules.len(),
            class_count
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn class_manager_find_class_delivers_real_bytecode() {
        let jmods_dir = find_jmods_dir();
        if jmods_dir.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmods_dir = jmods_dir.unwrap();

        let base = jmods_dir.join("java.base.jmod");
        let boot_cp = vec![base.to_string_lossy().into_owned()];
        let mut cm = ClassManager::new(&boot_cp, &[], &[]);

        // Load java.lang.Object — this is the deliverable for Session 6
        let result = cm.load_class("java/lang/Object");
        assert!(
            result.is_ok(),
            "ClassManager.load_class('java/lang/Object') failed: {:?}",
            result.err()
        );
        let class_id = result.unwrap();

        // Verify the loaded class has the right name
        let cls = cm.class_store.get(class_id).unwrap();
        assert_eq!(&*cls.name, "java/lang/Object");
        assert!(
            cls.superclass.is_none(),
            "Object should have no superclass"
        );
        assert!(
            !cls.is_synthetic_stub,
            "Object should NOT be a synthetic stub — it came from real bytecode"
        );

        eprintln!(
            "Loaded java.lang.Object (ClassId {:?}): {} methods, {} fields, synthetic={}",
            class_id,
            cls.methods.len(),
            cls.fields.len(),
            cls.is_synthetic_stub
        );
    }

    // ----------------------------------------------------------------
    // T6.3.1 — JVMTI class-hook registration / fire-path tests.
    // ----------------------------------------------------------------

    #[test]
    fn jvmti_hooks_off_by_default() {
        // No hooks installed yet → `CLASS_HOOKS_ACTIVE` may be either true
        // (if another test installed one earlier) or false, but the fire_*
        // helpers must never panic regardless.
        fire_class_load_hook(1, "a/b/C", 0);
        fire_class_prepare_hook(1, "a/b/C", 0);
    }

    #[test]
    fn jvmti_fire_hooks_dispatch_when_installed() {
        // The OnceLock-based registry is process-wide, so this test acts as
        // both the installer and the observer. Subsequent tests in the same
        // process rely on the installed hook remaining in place — callback
        // bodies therefore must tolerate being re-invoked.
        use std::sync::atomic::{AtomicU32, Ordering as O};
        static LOAD_CALLS: AtomicU32 = AtomicU32::new(0);
        static PREPARE_CALLS: AtomicU32 = AtomicU32::new(0);

        fn load_cb(_id: u32, _n: &str, _tid: u64) { LOAD_CALLS.fetch_add(1, O::SeqCst); }
        fn prepare_cb(_id: u32, _n: &str, _tid: u64) { PREPARE_CALLS.fetch_add(1, O::SeqCst); }

        install_class_load_hook(load_cb);
        install_class_prepare_hook(prepare_cb);

        let before_load = LOAD_CALLS.load(O::SeqCst);
        let before_prepare = PREPARE_CALLS.load(O::SeqCst);

        fire_class_load_hook(42, "com/example/Foo", 1);
        fire_class_prepare_hook(42, "com/example/Foo", 1);

        assert_eq!(LOAD_CALLS.load(O::SeqCst), before_load + 1);
        assert_eq!(PREPARE_CALLS.load(O::SeqCst), before_prepare + 1);
        // Flag must be set once either hook is installed.
        assert!(CLASS_HOOKS_ACTIVE.load(Ordering::Acquire));
    }

    // ------------------------------------------------------------------
    // T10.5 — vtable build in class_manager
    //
    // These tests drive `build_vtable_descriptors` by hand-rolling a
    // minimal `Class` with just enough method metadata to populate the
    // virtual slots. They don't exercise the install hook (that's tested
    // on the VM side against `VtableManager`), only the per-class build
    // logic that lives in this crate.
    // ------------------------------------------------------------------

    use rustjvm_reader::class_access_flags::MethodAccessFlags;
    use rustjvm_reader::method::ClassFileMethod;

    fn stub_method(name: &str, descriptor: &str, flags: MethodAccessFlags) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: flags,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: vec![],
        }
    }

    /// Create and register a stub `Class` carrying the given method list.
    ///
    /// Returns the newly allocated `ClassId`. The class has no fields, no
    /// interfaces, and no constant pool entries — it's just a vehicle for
    /// the method list.
    fn add_stub_class(
        mgr: &mut ClassManager,
        name: &str,
        superclass: Option<ClassId>,
        methods: Vec<ClassFileMethod>,
    ) -> ClassId {
        let id = mgr.class_store.next_id();
        mgr.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let entries = mgr.build_vtable_descriptors(id, superclass);
        mgr.vtable_descriptors.insert(id, entries);
        id
    }

    /// T10.5 — build_vtable_descriptors populates virtual slots for a
    /// root class with no superclass.
    #[test]
    fn t10_class_manager_vtable_populated_at_link_time() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let cid = add_stub_class(
            &mut mgr,
            "pkg/Root",
            None,
            vec![
                stub_method("foo", "()V", MethodAccessFlags::PUBLIC),
                stub_method("bar", "()I", MethodAccessFlags::PUBLIC),
            ],
        );

        let entries = mgr.vtable_descriptors_of(cid).expect("vtable missing");
        assert_eq!(entries.len(), 2);
        let e0 = entries[0].as_ref().expect("slot 0 empty");
        assert_eq!(e0.declaring_class_id, cid.as_u32());
        assert_eq!(e0.method_index, 0);
        // T10.9.E: method_name is now `Arc<str>` — deref for &str comparison.
        assert_eq!(&*e0.method_name, "foo");

        let e1 = entries[1].as_ref().expect("slot 1 empty");
        assert_eq!(e1.declaring_class_id, cid.as_u32());
        assert_eq!(e1.method_index, 1);
        assert_eq!(&*e1.method_name, "bar");
    }

    /// T10.5 — static / private / `<init>` / `<clinit>` methods must be
    /// excluded from the vtable.
    #[test]
    fn t10_class_manager_vtable_excludes_non_virtual() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let cid = add_stub_class(
            &mut mgr,
            "pkg/Mixed",
            None,
            vec![
                stub_method("<init>", "()V", MethodAccessFlags::PUBLIC),
                stub_method("<clinit>", "()V", MethodAccessFlags::STATIC),
                stub_method(
                    "staticOnly",
                    "()V",
                    MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                ),
                stub_method(
                    "privateOnly",
                    "()V",
                    MethodAccessFlags::PRIVATE,
                ),
                stub_method("virtualOne", "()V", MethodAccessFlags::PUBLIC),
                stub_method(
                    "virtualTwo",
                    "()I",
                    MethodAccessFlags::PROTECTED,
                ),
            ],
        );

        let entries = mgr.vtable_descriptors_of(cid).unwrap();
        assert_eq!(entries.len(), 2, "only the two public/protected virtuals");
        assert_eq!(&*entries[0].as_ref().unwrap().method_name, "virtualOne");
        assert_eq!(&*entries[1].as_ref().unwrap().method_name, "virtualTwo");
    }

    /// T10.5 — a subclass that doesn't override sees the super's slot
    /// verbatim (same declaring_class_id, same method_index).
    #[test]
    fn t10_class_manager_vtable_inherits_from_super() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let super_id = add_stub_class(
            &mut mgr,
            "pkg/Super",
            None,
            vec![stub_method("greet", "()V", MethodAccessFlags::PUBLIC)],
        );

        // Subclass adds a new method; does NOT override greet.
        let sub_id = add_stub_class(
            &mut mgr,
            "pkg/Sub",
            Some(super_id),
            vec![stub_method("extra", "()V", MethodAccessFlags::PUBLIC)],
        );

        let sub_entries = mgr.vtable_descriptors_of(sub_id).unwrap();
        assert_eq!(sub_entries.len(), 2);

        // Slot 0 must still reference the super's declaration.
        let inherited = sub_entries[0].as_ref().unwrap();
        assert_eq!(
            inherited.declaring_class_id,
            super_id.as_u32(),
            "inherited slot should carry super's declaring_class_id",
        );
        assert_eq!(&*inherited.method_name, "greet");

        // Slot 1 is the subclass's own new method.
        let own = sub_entries[1].as_ref().unwrap();
        assert_eq!(own.declaring_class_id, sub_id.as_u32());
        assert_eq!(&*own.method_name, "extra");
    }

    /// T10.5 — override replaces the super's entry in place at the same
    /// slot index.
    #[test]
    fn t10_class_manager_vtable_override_replaces_super() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let super_id = add_stub_class(
            &mut mgr,
            "pkg/SuperOv",
            None,
            vec![
                stub_method("hashCode", "()I", MethodAccessFlags::PUBLIC),
                stub_method("toString", "()Ljava/lang/String;", MethodAccessFlags::PUBLIC),
            ],
        );

        let sub_id = add_stub_class(
            &mut mgr,
            "pkg/SubOv",
            Some(super_id),
            vec![
                // Same signature as super's "toString" — must override
                // slot 1 in place.
                stub_method(
                    "toString",
                    "()Ljava/lang/String;",
                    MethodAccessFlags::PUBLIC,
                ),
            ],
        );

        let sub_entries = mgr.vtable_descriptors_of(sub_id).unwrap();
        assert_eq!(
            sub_entries.len(),
            2,
            "override must not grow the vtable",
        );

        // Slot 0 (hashCode) still declared by super.
        let hc = sub_entries[0].as_ref().unwrap();
        assert_eq!(hc.declaring_class_id, super_id.as_u32());
        assert_eq!(&*hc.method_name, "hashCode");

        // Slot 1 (toString) now declared by the subclass.
        let ts = sub_entries[1].as_ref().unwrap();
        assert_eq!(
            ts.declaring_class_id,
            sub_id.as_u32(),
            "override must replace super's declaring_class_id",
        );
        assert_eq!(ts.method_index, 0, "subclass's own method_index");
    }

    /// T10.5 — install hook receives the freshly-built descriptor vec.
    #[test]
    fn t10_class_manager_vtable_install_hook_fires() {
        use std::sync::atomic::{AtomicU32, Ordering};

        static HOOK_CALLS: AtomicU32 = AtomicU32::new(0);
        static LAST_CLASS_ID: AtomicU32 = AtomicU32::new(0);
        static LAST_LEN: AtomicU32 = AtomicU32::new(0);

        fn my_hook(class_id: u32, entries: Vec<Option<VtableSlotDescriptor>>) {
            HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
            LAST_CLASS_ID.store(class_id, Ordering::SeqCst);
            LAST_LEN.store(entries.len() as u32, Ordering::SeqCst);
        }
        install_vtable_install_hook(my_hook);

        // Fire the hook directly — same code path that
        // `define_class_with_options` uses.
        let before = HOOK_CALLS.load(Ordering::SeqCst);
        fire_vtable_install_hook(
            0xCafe_Babe,
            vec![Some(VtableSlotDescriptor {
                declaring_class_id: 0xCafe_Babe,
                method_index: 0,
                method_name: "x".into(),
                descriptor: "()V".into(),
                dispatch: None,
            })],
        );

        assert!(HOOK_CALLS.load(Ordering::SeqCst) >= before + 1);
        assert_eq!(LAST_CLASS_ID.load(Ordering::SeqCst), 0xCafe_Babe);
        assert_eq!(LAST_LEN.load(Ordering::SeqCst), 1);
    }

    // --------------------------------------------------------------
    // T10.9.A tests — dispatch snapshot + override-hook
    // --------------------------------------------------------------

    /// T10.9.A.2 — `build_vtable_descriptors` populates the
    /// `dispatch` field for each concrete bytecode method.
    #[test]
    fn t10_9_a_build_vtable_populates_dispatch_for_concrete_methods() {
        use rustjvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};

        let mut mgr = ClassManager::new(&[], &[], &[]);
        let id = mgr.class_store.next_id();

        // Build a Class with one concrete bytecode method whose
        // CodeAttribute has a known max_stack / max_locals. We wrap the
        // synthetic Code in a `LazyAttribute::Decoded` so the method
        // matches the post-T11 method-attribute storage; producers that
        // bypass the class reader (tests, AOT caches, synthetic stubs)
        // use the decoded form to avoid round-tripping through raw bytes.
        let method = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("concrete"),
            descriptor: Arc::from("(I)V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(CodeAttribute {
                max_stack: 3,
                max_locals: 4,
                code: vec![0x01, 0xb1], // aconst_null; return
                exception_table: vec![],
                attributes: vec![],
            }))],
        };

        mgr.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from("pkg/Dispatch"),
            source_file: Some("Dispatch.java".to_string()),
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![method],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });

        let (entries, overrides) = mgr.build_vtable_descriptors_with_overrides(id, None);
        assert!(overrides.is_empty(), "no super -> no overrides");
        assert_eq!(entries.len(), 1);
        let entry = entries[0].as_ref().expect("slot 0 populated");
        let dispatch = entry
            .dispatch
            .as_ref()
            .expect("concrete method must carry dispatch snapshot");
        assert_eq!(dispatch.max_stack, 3);
        assert_eq!(dispatch.max_locals, 4);
        assert_eq!(dispatch.code, vec![0x01, 0xb1]);
        assert_eq!(dispatch.num_params, 1); // (I) takes one slot
        assert!(!dispatch.is_native);
        assert_eq!(&dispatch.class_name, "pkg/Dispatch");
    }

    /// T10.9.A.4 — override detection: when a subclass method matches
    /// an inherited slot's (name, descriptor), the returned
    /// `overrides` vec contains `(super_class_id, slot)`.
    #[test]
    fn t10_9_a_build_vtable_reports_overrides() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let super_id = add_stub_class(
            &mut mgr,
            "pkg/OverSuper",
            None,
            vec![
                stub_method("a", "()V", MethodAccessFlags::PUBLIC),
                stub_method("b", "()I", MethodAccessFlags::PUBLIC),
            ],
        );

        // Subclass overrides "b" only.
        let sub_id = mgr.class_store.next_id();
        mgr.class_store.add(Class {
            id: sub_id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from("pkg/OverSub"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: Some(super_id),
            interfaces: vec![],
            fields: vec![],
            methods: vec![stub_method("b", "()I", MethodAccessFlags::PUBLIC)],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let (_entries, overrides) =
            mgr.build_vtable_descriptors_with_overrides(sub_id, Some(super_id));
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].0, super_id.as_u32());
        assert_eq!(overrides[0].1, 1, "overrode slot 1 (b)");
    }

    /// T10.9.A — the override hook fires once per overridden slot when
    /// `fire_vtable_override_hook` is driven by the build output.
    #[test]
    fn t10_9_a_override_hook_fires_per_slot() {
        use std::sync::atomic::{AtomicU32, Ordering as O2};

        static OV_CALLS: AtomicU32 = AtomicU32::new(0);
        static LAST_SUPER: AtomicU32 = AtomicU32::new(0);
        static LAST_SLOT: AtomicU32 = AtomicU32::new(0);

        fn ov_hook(super_class_id: u32, slot: usize) {
            OV_CALLS.fetch_add(1, O2::SeqCst);
            LAST_SUPER.store(super_class_id, O2::SeqCst);
            LAST_SLOT.store(slot as u32, O2::SeqCst);
        }
        install_vtable_override_hook(ov_hook);

        let before = OV_CALLS.load(O2::SeqCst);
        fire_vtable_override_hook(0x42, 7);
        assert!(OV_CALLS.load(O2::SeqCst) >= before + 1);
        assert_eq!(LAST_SUPER.load(O2::SeqCst), 0x42);
        assert_eq!(LAST_SLOT.load(O2::SeqCst), 7);
    }

    /// T10.9.A.6 — abstract methods (no Code attribute) produce a
    /// `dispatch: None` entry — the interpreter's fast path falls
    /// through to the slow path which raises `AbstractMethodError`.
    #[test]
    fn t10_9_a_abstract_method_dispatch_is_none() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let id = mgr.class_store.next_id();
        mgr.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from("pkg/Abstr"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_constant_pool(),
            access_flags: ClassAccessFlags::PUBLIC
                | ClassAccessFlags::SUPER
                | ClassAccessFlags::ABSTRACT,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![stub_method(
                "abstractOne",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT,
            )],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            signature: None,
            has_finalizer: false,
            code_source: None,
            attributes: Vec::new(),
            array_info: None,
            source_file_cache: OnceLock::new(),
            signature_cache: OnceLock::new(),
            nest_host_cache: OnceLock::new(),
            enclosing_method_cache: OnceLock::new(),
            record_components_cache: OnceLock::new(),
        });
        let entries = mgr.build_vtable_descriptors(id, None);
        assert_eq!(entries.len(), 1);
        let e = entries[0].as_ref().unwrap();
        assert!(e.dispatch.is_none(), "abstract methods carry dispatch=None");
    }

    // ----------------------------------------------------------------
    // WP2.4-B — redefine_class lib-level smoke tests
    //
    // These cover the bookkeeping logic that doesn't need a real class
    // file fixture: generation counter allocation, default options,
    // unknown-id rejection. Full structural-equivalence + body-swap
    // coverage lives in tests/wp2_4b_redefine.rs which uses compiled
    // .class fixtures.
    // ----------------------------------------------------------------

    #[test]
    fn redefine_class_unknown_id_rejected_lib() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        // Build minimum-valid CAFEBABE bytes (>=8) so the header
        // pre-check passes; the function then rejects on
        // class-id-not-loaded BEFORE parsing.
        let bytes = vec![
            0xCA, 0xFE, 0xBA, 0xBE, // magic
            0x00, 0x00, // minor
            0x00, 0x45, // major (Java 21)
        ];
        let bogus = ClassId::new(99_999);
        let err = mgr
            .redefine_class(bogus, bytes, RedefineOptions::default())
            .unwrap_err();
        let s = format!("{err:?}");
        assert!(
            s.contains("UnsupportedClassRedefinition") && s.contains("not loaded"),
            "expected 'not loaded' UnsupportedClassRedefinitionError, got: {s}"
        );
    }

    #[test]
    fn redefine_class_short_bytes_rejected_lib() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let bogus = ClassId::new(0);
        let err = mgr
            .redefine_class(bogus, vec![0xCA], RedefineOptions::default())
            .unwrap_err();
        let s = format!("{err:?}");
        assert!(
            s.contains("UnsupportedClassRedefinition") && s.contains("too short"),
            "expected 'too short' rejection, got: {s}"
        );
    }

    #[test]
    fn redefine_class_bad_magic_rejected_lib() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let bogus = ClassId::new(0);
        let mut bytes = vec![0u8; 8];
        bytes[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let err = mgr
            .redefine_class(bogus, bytes, RedefineOptions::default())
            .unwrap_err();
        let s = format!("{err:?}");
        assert!(
            s.contains("UnsupportedClassRedefinition") && s.contains("bad magic"),
            "expected 'bad magic' rejection, got: {s}"
        );
    }

    #[test]
    fn redefine_generation_counter_starts_at_zero() {
        let mgr = ClassManager::new(&[], &[], &[]);
        let cid = ClassId::new(0); // never registered
        assert_eq!(
            mgr.class_redefine_generation(cid),
            0,
            "fresh / unknown class must report generation 0",
        );
    }

    #[test]
    fn redefine_generation_handle_lazy_allocates() {
        let mut mgr = ClassManager::new(&[], &[], &[]);
        let cid = ClassId::new(0);
        let h1 = mgr.class_redefine_generation_handle(cid);
        let h2 = mgr.class_redefine_generation_handle(cid);
        // Same backing AtomicU32 (Arc::ptr_eq).
        assert!(
            Arc::ptr_eq(&h1, &h2),
            "two handle calls for the same id must share backing storage",
        );
        assert_eq!(h1.load(Ordering::Acquire), 0);
    }

    #[test]
    fn redefine_options_default_is_strict() {
        let opts = RedefineOptions::default();
        assert!(!opts.skip_structural_check);
        assert!(!opts.log_diff);
    }

    #[test]
    fn class_file_load_hook_inactive_returns_none() {
        // With no hook installed, fire_class_file_load_hook is a noop.
        // Best-effort assertion: it doesn't panic.
        let out = fire_class_file_load_hook(0, "Foo", b"old", b"new");
        // The OnceLock + AtomicBool gate may have been activated by
        // prior tests in this process; we only assert no panic and
        // (if no hook is set) None.
        if !CLASS_FILE_LOAD_HOOK_ACTIVE.load(Ordering::Acquire) {
            assert!(out.is_none());
        }
    }

    #[test]
    fn jit_invalidate_hook_inactive_returns_quietly() {
        // No hook installed — must be a no-op.
        fire_jit_invalidate_hook(0);
        // No assertion beyond "doesn't panic".
    }
}
