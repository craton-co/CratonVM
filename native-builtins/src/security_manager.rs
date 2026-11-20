// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! SecurityManager, AccessController, and AccessControlContext native implementations.
//!
//! SecurityManager is deprecated for removal (JEP 411, Java 17+) but JDK 25 still
//! supports the API. When a `java.policy` file is loaded, permissions are checked
//! against parsed grants. Otherwise, the default is allow-all (matching JDK behavior
//! when a SecurityManager is installed programmatically without a policy file).
//!
//! AccessController.doPrivileged genuinely invokes action.run() via ctx.invoke_virtual
//! and pushes a frame onto a per-thread privileged-frame stack so that permission
//! checks can find the code source associated with the privileged call.

use std::cell::RefCell;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ClassId, ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

pub mod policy;
pub mod x509;
pub use policy::{Grant, PermissionEntry, Policy, PolicyError};

// ---------------------------------------------------------------------------
// Cached `CRATONVM_DBG_DOPRIV` env-var lookup
//
// `doPrivileged` is called ~50k times during JDK boot; reading
// `env::var_os` per call funnels every thread through the platform
// environ lock and reallocates an `OsString`. Cache the boolean once at
// first use — same pattern as `vm::runtime::exceptions::iae_trace_enabled`.
// The env var is a debug switch and must be set before `doPrivileged`
// is first invoked.
static DBG_DOPRIV: OnceLock<bool> = OnceLock::new();

#[inline]
fn dbg_dopriv_enabled() -> bool {
    *DBG_DOPRIV.get_or_init(|| crate::nbflags().dbg_dopriv)
}

// SECURITY FIX (V10): strict opt-in for the defence-in-depth profile.
//
// Default runtime behavior: "no policy loaded = no enforcement" — when no
// java.policy is installed the VM allows everything, matching JDK semantics
// and the no-SecurityManager compatibility path. This is intentionally NOT
// changed, because silently denying with no policy would break the
// JDK-compat case.
//
// When `CRATONVM_REQUIRE_POLICY` is set, a missing policy instead DENIES
// (fail-closed). `CRATONVM_UNTRUSTED_CODE` implies this flag so that the
// absence of an explicitly-loaded policy can never be mistaken for an
// allow-all grant. The flag must be set before the first permission check.
static REQUIRE_POLICY: OnceLock<bool> = OnceLock::new();

#[inline]
fn require_policy_enabled() -> bool {
    *REQUIRE_POLICY.get_or_init(|| {
        crate::nbflags().require_policy || cratonvm_types::flags::flags().io.untrusted_code
    })
}

/// Enforce the common boundary for JNI library loads, symbol lookup, and FFM
/// host calls. The untrusted-code profile always denies native execution.
/// Otherwise an installed SecurityManager must grant the corresponding
/// `RuntimePermission("loadLibrary.<target>")`.
pub fn check_host_native_access_or_throw(
    ctx: &mut dyn NativeContext,
    target: &str,
) -> Result<(), MethodCallFailed> {
    let permission_target = format!("loadLibrary.{target}");
    if cratonvm_types::flags::flags().io.untrusted_code {
        return Err(RuntimeError::SecurityException {
            message: format!(
                "host native access denied by CRATONVM_UNTRUSTED_CODE ({permission_target})"
            ),
        }
        .into());
    }
    if get_security_manager(&*ctx).is_none() {
        return Ok(());
    }
    let code_base = current_privileged_code_base_arc();
    let cert_digests = current_privileged_cert_digests_arc();
    if policy_allows_full_generic(
        "java/lang/RuntimePermission",
        &permission_target,
        "",
        code_base.as_deref(),
        &cert_digests,
    ) {
        Ok(())
    } else {
        Err(throw_access_control_exception(
            ctx,
            access_denied_text("java/lang/RuntimePermission", &permission_target, "", false),
        ))
    }
}

/// `AccessControlException`'s text, `"access denied " + perm`, with
/// `Permission.toString()`'s spelling: the class's binary (dotted) name, and
/// the actions clause only when the actions are non-empty. Round 15 wave 7
/// (lane drainA; F5 of `r15w4-jdkonly5-review-findings`, the wording half --
/// whether the manager survives JEP 486 stays the owner's call): this printed
/// the internal name and always an actions clause (`""` for a
/// `BasicPermission`). `CRATONVM_ACCESS_DENIED_PERMISSION_TEXT=0` restores it
/// (`legacy_actions_clause`: whether the site printed the clause before).
fn access_denied_text(
    class_name: &str,
    target: &str,
    actions: &str,
    legacy_actions_clause: bool,
) -> String {
    let hotspot =
        true;
    let class_name = if hotspot {
        class_name.replace('/', ".")
    } else {
        class_name.to_string()
    };
    let clause = if hotspot {
        !actions.is_empty()
    } else {
        legacy_actions_clause
    };
    if !clause {
        format!("access denied (\"{class_name}\" \"{target}\")")
    } else {
        format!("access denied (\"{class_name}\" \"{target}\" \"{actions}\")")
    }
}

// ---------------------------------------------------------------------------
// Per-VM SecurityManager / Policy state
// ---------------------------------------------------------------------------

/// The heap objects this module owns, for ONE VM.
///
/// Every slot is the object's CURRENT address: the owning VM's collector roots
/// it ([`gc_scan_security_manager_roots`]) and repoints it after a move
/// ([`gc_update_security_manager_refs`]), both through the
/// `vm/src/memory/native_roots.rs` `VM_ROOT_SOURCES` row. The slots are per-VM
/// — that is the whole point of the struct, see [`SECURITY_STATE`].
///
/// gc-common w29-d: a slot used to be `(identity_key, ObjectRef)`, rooted a
/// second time through `register_var_handle_root` and re-read through
/// `read_var_handle_root(identity_key)`. That registry is keyed by identity
/// hash, so a same-hash object registered by any other native made the read
/// answer THAT object (the `SecurityManager` slot could return an unrelated
/// object), and every manager / policy ever installed stayed rooted forever.
/// The collector-maintained copy is the only one now.
#[derive(Clone, Copy, Default)]
struct VmSecurityState {
    /// The `java.lang.SecurityManager` installed by `System.setSecurityManager`.
    security_manager: Option<ObjectRef>,
    /// The `java.security.Policy` installed by `Policy.setPolicy`, or the
    /// lazily-materialised synthetic default.
    policy_object: Option<ObjectRef>,
    /// The shared read-only `Permissions` collection handed out by
    /// `Policy.getPermissions(...)`.
    shared_permissions: Option<ObjectRef>,
}

/// Names one field of [`VmSecurityState`]. Accessors take this instead of a
/// field-projecting closure so every read and write goes through the same
/// VM-keyed lookup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    SecurityManager,
    PolicyObject,
    SharedPermissions,
}

impl VmSecurityState {
    fn slot(&self, slot: Slot) -> Option<ObjectRef> {
        match slot {
            Slot::SecurityManager => self.security_manager,
            Slot::PolicyObject => self.policy_object,
            Slot::SharedPermissions => self.shared_permissions,
        }
    }

    fn set_slot(&mut self, slot: Slot, value: Option<ObjectRef>) {
        match slot {
            Slot::SecurityManager => self.security_manager = value,
            Slot::PolicyObject => self.policy_object = value,
            Slot::SharedPermissions => self.shared_permissions = value,
        }
    }

    /// True once every slot is empty, so the VM's row can be dropped instead of
    /// kept as an all-`None` shell.
    fn is_empty(&self) -> bool {
        self.security_manager.is_none()
            && self.policy_object.is_none()
            && self.shared_permissions.is_none()
    }
}

/// Process-global INDEX of per-VM security state, keyed by
/// `NativeContext::vm_identity()`.
///
/// It is an index, not a policy: nothing in it is reachable without a
/// `NativeContext`, so a cross-VM read is unrepresentable at the call sites.
/// `native-api/src/capability.rs`'s `VM_CAPABILITIES` is the same shape and the
/// model this follows.
///
/// It replaces three process-global `Mutex<Option<(i32, ObjectRef)>>`
/// singletons — the SecurityManager, the `Policy` object, and the shared
/// `Permissions` collection — which were wrong twice over:
///
///   * **Memory safety.** The rooting they leaned on
///     (`register_var_handle_root` → `HeapRealm::var_handle_roots`) is PER-VM,
///     so the `read_var_handle_root(key).unwrap_or(cached)` read MISSED in any
///     other VM and handed back the installing VM's raw address. That address
///     belongs to a heap the reading VM's collector never scans and the owning
///     VM's collector cannot rewrite (it rewrites the registry entry, not the
///     static copy) — a use-after-move under a moving young GC, and exactly the
///     unrewritable holder `docs/threading/objectref-concurrency-contract.md`
///     forbids.
///   * **Sandboxing.** `System.setSecurityManager(null)` wrote `None` to the
///     shared slot and thereby disarmed EVERY other VM's `checkExec` /
///     `loadLibrary` gate, so an unsandboxed VM could unarm a sandboxed one
///     unchallenged.
///
/// Keying on VM identity closes the second; rooting the held refs through
/// [`gc_scan_security_manager_roots`] / [`gc_update_security_manager_refs`]
/// (the `lang_math::gc_scan_value_of_cache_roots` shape) closes the first by
/// making the cached copy itself collector-visible AND rewritable. Since
/// gc-common w29-d that copy is the only one: the var-handle-root registration
/// is gone (see [`VmSecurityState`]).
// addr-keyed: not a heap address -- VM identity.
static SECURITY_STATE: OnceLock<Mutex<std::collections::HashMap<usize, VmSecurityState>>> =
    OnceLock::new();

// addr-keyed: the accessor of SECURITY_STATE (see its disposition).
fn security_state() -> &'static Mutex<std::collections::HashMap<usize, VmSecurityState>> {
    SECURITY_STATE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Run `f` with the state table locked.
///
/// The lock is NEVER held across a Java allocation or any other re-entry into
/// the VM: [`gc_scan_security_manager_roots`] takes the same lock at a
/// safepoint, so a thread that allocated while holding it could deadlock
/// against its own collection. The two lazy initialisers below therefore
/// allocate first and publish afterwards.
fn with_security_state<R>(
    // addr-keyed: a parameter; SECURITY_STATE carries the disposition.
    f: impl FnOnce(&mut std::collections::HashMap<usize, VmSecurityState>) -> R,
) -> R {
    let mut guard = security_state().lock().unwrap_or_else(|e| e.into_inner());
    let out = f(&mut guard);
    // JIT round 16 wave 14 (lane ffm3, FFS16-1): refresh the mirror under the
    // same lock that changed the table (a handful of rows: one per VM).
    let installed = guard
        .values()
        .filter(|state| state.security_manager.is_some())
        .count();
    SECURITY_MANAGERS_INSTALLED.store(installed, std::sync::atomic::Ordering::Release);
    out
}

/// JIT round 16 wave 14 (lane ffm3, proposal FFS16-1): how many VMs have a
/// `SecurityManager` in their slot of [`SECURITY_STATE`], rewritten under its
/// lock by every access ([`with_security_state`]). An INDEX mirror, the shape
/// of `capability.rs`'s `VM_CAPABILITIES_LEN`, not compatibility state: it
/// names no VM and no object, and it answers one question without the
/// process-wide mutex -- "can any VM's `getSecurityManager` answer non-null?"
/// -- for the FFM memo paths' per-call access verdict
/// (`panama::downcall_access_settled`, `CRATONVM_FFM_SECURITY_MANAGER_MIRROR`).
/// On JDK 25 it stays 0: `System.setSecurityManager` throws there.
static SECURITY_MANAGERS_INSTALLED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// JIT round 16 wave 14 (lane ffm3, FFS16-1): does any VM in the process hold a
/// `SecurityManager`? `false` means [`get_security_manager`] answers `None`
/// for every context, as of the last change to the table (a racing install is
/// the same race as reading the slot just before it).
pub fn any_security_manager_installed() -> bool {
    SECURITY_MANAGERS_INSTALLED.load(std::sync::atomic::Ordering::Acquire) != 0
}

/// The address of the word [`any_security_manager_installed`] reads (JIT
/// round 22, FFM22-1): compiled code that makes a bound FFM downcall itself
/// tests it for zero per call, as the direct row's access verdict does
/// (`panama::compiled_call_access_guard`).
pub fn security_managers_installed_word() -> usize {
    std::ptr::addr_of!(SECURITY_MANAGERS_INSTALLED) as usize
}

/// Read one slot of the CALLING VM's state. Another VM's slot is not
/// addressable from here — the key comes from `ctx`, never from a caller.
fn security_slot(ctx: &dyn NativeContext, slot: Slot) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    with_security_state(|table| table.get(&vm).and_then(|state| state.slot(slot)))
}

/// Store (or clear) one slot of the CALLING VM's state. Clearing the last
/// occupied slot drops the VM's row entirely.
fn set_security_slot(ctx: &dyn NativeContext, slot: Slot, entry: Option<ObjectRef>) {
    let vm = ctx.vm_identity();
    with_security_state(|table| {
        if entry.is_some() {
            table.entry(vm).or_default().set_slot(slot, entry);
        } else if let Some(state) = table.get_mut(&vm) {
            state.set_slot(slot, None);
            if state.is_empty() {
                table.remove(&vm);
            }
        }
    });
}

/// Publish `entry` into the calling VM's `slot` unless another thread got there
/// first; return whichever entry is installed afterwards. Used by the two lazy
/// initialisers, whose allocation deliberately happens before this call.
fn publish_security_slot(ctx: &dyn NativeContext, slot: Slot, entry: ObjectRef) -> ObjectRef {
    let vm = ctx.vm_identity();
    with_security_state(|table| {
        let state = table.entry(vm).or_default();
        match state.slot(slot) {
            Some(existing) => existing,
            None => {
                state.set_slot(slot, Some(entry));
                entry
            }
        }
    })
}

/// GC root scan hook — companion to [`gc_update_security_manager_refs`].
///
/// Reports the SecurityManager, the `Policy` object and the shared
/// `Permissions` collection held for `vm_identity`, so the owning collector
/// relocates rather than reclaims them. Scoped to one VM: a heap address only
/// means anything inside the heap that produced it, and handing another VM's
/// address to this collector would be a pointer into a heap it does not own.
///
/// Takes a blocking lock that is never held across a Java allocation, so the
/// allocating thread cannot self-deadlock here.
pub fn gc_scan_security_manager_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    with_security_state(|table| {
        let Some(state) = table.get(&vm_identity) else {
            return;
        };
        for slot in [
            state.security_manager,
            state.policy_object,
            state.shared_permissions,
        ] {
            if let Some(obj) = slot {
                out.push(obj);
            }
        }
    });
}

/// GC post-compaction hook — companion to [`gc_scan_security_manager_roots`].
///
/// Repoints every slot held for `vm_identity` through the collector's old→new
/// map. This is the half that was missing: the var-handle-root REGISTRY entry
/// was remapped but the cached copy never was, so after a move the cache
/// pointed at a vacated from-space slot. Identity keys are untouched — an
/// identity hash is stable across a move.
pub fn gc_update_security_manager_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    with_security_state(|table| {
        let Some(state) = table.get_mut(&vm_identity) else {
            return;
        };
        for slot in [
            &mut state.security_manager,
            &mut state.policy_object,
            &mut state.shared_permissions,
        ] {
            if let Some(obj_ref) = slot {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    // SAFETY: `new_addr` is the post-move address the calling
                    // collector just assigned to this very object, taken from
                    // its own relocation map. Same construction as
                    // `lang_math::gc_update_value_of_cache_refs`.
                    *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    });
}

/// Per-VM teardown: drop every security slot held for `vm_identity`.
///
/// Call when a VM is disposed of. Without it the row — and the raw heap
/// addresses in it — outlives the heap that produced them, and a later VM that
/// reused the identity would inherit a dead SecurityManager. Nothing else in
/// the process holds these refs: they are not shared across VMs by
/// construction.
pub fn forget_vm_security_state(vm_identity: usize) {
    with_security_state(|table| {
        table.remove(&vm_identity);
    });
}

#[cfg(test)]
static SECURITY_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Serialize tests that mutate process-wide SecurityManager or policy state.
///
/// Holding the singleton's own mutex across a test would deadlock when the
/// code under test reads it, so the harness uses this independent lock.
#[cfg(test)]
pub fn security_state_test_lock() -> std::sync::MutexGuard<'static, ()> {
    SECURITY_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Return the `java.lang.SecurityManager` installed in the CALLING VM, or
/// `None` if `System.setSecurityManager(null)` is in effect there (the
/// default).
///
/// `pub` so security-sensitive native entry points can consult the same
/// slot: `ProcessBuilder.start` / `Runtime.exec*` via
/// `lang_system::check_exec_or_throw`, and the Panama host-call gate via
/// `panama::check_native_access`.
///
/// The answer is scoped to `ctx`: another VM's manager is neither readable nor
/// clearable from here, so one VM can no longer disarm another's gates.
pub fn get_security_manager(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    // The slot is the CURRENT address: the collector roots and repoints it
    // (see `VmSecurityState`).
    security_slot(ctx, Slot::SecurityManager)
}

fn set_security_manager(ctx: &mut dyn NativeContext, sm: Option<ObjectRef>) {
    // Kept alive and repointed across GC moves by this module's own
    // `VM_ROOT_SOURCES` row (`gc_scan_security_manager_roots`).
    set_security_slot(&*ctx, Slot::SecurityManager, sm);
}

/// Override the calling VM's SecurityManager slot for tests. Lets unit tests
/// install a synthetic SM object so they can exercise the checkExec
/// gating path without going through `System.setSecurityManager`.
///
/// Returns the previous value so callers can restore it on tear-down.
///
/// `ctx` is required for the same reason production callers need one: the slot
/// belongs to a VM, and there is no "current VM" to infer.
#[cfg(test)]
pub fn set_security_manager_for_test(
    ctx: &dyn NativeContext,
    sm: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    with_security_state(|table| {
        let state = table.entry(vm).or_default();
        let prev = state.security_manager;
        state.security_manager = sm;
        if state.is_empty() {
            table.remove(&vm);
        }
        prev
    })
}

// ---------------------------------------------------------------------------
// T19_H9_ANCHOR_POLICY_SINGLETON
// Per-VM java.security.Policy singleton
// ---------------------------------------------------------------------------
//
// Mirrors the layout of HotSpot's `Policy.policyInfo` static slot: a single
// per-VM reference that `Policy.setPolicy(Policy)` writes to and
// `Policy.getPolicy()` reads from. Real JDK 25 throws
// `UnsupportedOperationException` from these entry points (JEP 411 sealing
// the SecurityManager surface), but cratonvm's lenient model accepts the
// installation: we simply hold the reference so callers like JBoss Modules,
// WildFly, and EJBCA — which call `Policy.setPolicy(new ModulesPolicy())`
// during boot — can proceed.
//
// Storage is a plain `Option<...>` slot in the VM's row, not a `OnceLock`,
// because it must be reassignable. `setPolicy(null)` clears the slot and the
// next `getPolicy()` call lazily re-creates the synthetic default — no
// null-deref window can occur because publication of the lazy default is a
// single atomic step under the state mutex (the ALLOCATION deliberately
// happens outside it; see `ensure_default_policy_object`).
//
// The singleton is per-VM, and within one VM it is process-wide in the sense
// the JDK means: a hostile caller from inside that JVM can read or overwrite
// it, mirroring real JDK behaviour, and that is intentional. What it is NOT is
// shared with a SECOND VM in the same process — see [`SECURITY_STATE`] for why
// that was both a use-after-move and a sandbox hole.
//
// GC: stored as the object's address in the owning VM's row, scanned and
// repointed directly by `gc_scan_security_manager_roots` /
// `gc_update_security_manager_refs`, so the stored address is always current.
// (gc-common w29-d dropped the second rooting through the identity-hash-keyed
// var-handle-root registry; see `VmSecurityState`.)

/// Read the Java `Policy` object installed in the CALLING VM, if any. The slot
/// holds the CURRENT (post-GC) address.
fn get_policy_object(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    security_slot(ctx, Slot::PolicyObject)
}

/// Cheap "is a Policy installed in this VM?" probe that does not touch object
/// addresses (used by `Policy.isSet`, which only needs presence).
fn policy_object_installed(ctx: &dyn NativeContext) -> bool {
    security_slot(ctx, Slot::PolicyObject).is_some()
}

/// Store a new Java `Policy` reference for the calling VM (or clear with
/// `None`). This matches `Policy.setPolicy(Policy)` semantics: `null` is
/// accepted and results in a future `getPolicy()` call lazily allocating the
/// synthetic default.
fn set_policy_object(ctx: &mut dyn NativeContext, p: Option<ObjectRef>) {
    // Kept alive and repointed by this module's `VM_ROOT_SOURCES` row.
    set_security_slot(&*ctx, Slot::PolicyObject, p);
}

/// Lazily allocate the calling VM's synthetic default Policy, used when no
/// caller in that VM has invoked `setPolicy(...)`.
///
/// The instance carries no per-Policy fields — `getPermissions(...)` is
/// answered from the VM's shared-`Permissions` slot directly so the default
/// Policy doesn't need its own cache slot. Real JDK's `Policy` has a
/// `pdMapping` field, but we deliberately leave it null because none of
/// our overridden natives read it.
///
/// The allocation happens BEFORE the state lock is taken, not under it: the GC
/// root scan takes the same lock at a safepoint, so holding it across
/// `alloc_concurrent_synthetic` (which can trigger a collection) would deadlock
/// the allocating thread against its own GC. Publication is still a single
/// atomic step, so no caller can observe a half-initialised default; if two
/// threads race, the loser drops its allocation and BOTH return the same
/// object, which is the identity guarantee callers actually depend on.
fn ensure_default_policy_object(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(existing) = get_policy_object(&*ctx) {
        return Ok(existing);
    }
    let p = try_alloc_concurrent_synthetic(ctx, "java/security/Policy", 0)?;
    // Published with no allocation in between, so `p` is still current; from
    // here on this module's `VM_ROOT_SOURCES` row keeps the slot alive and
    // current.
    Ok(publish_security_slot(&*ctx, Slot::PolicyObject, p))
}

/// Return the calling VM's read-only permissive `Permissions` collection,
/// lazily allocating it on first call. Subsequent calls in the same VM return
/// the same reference so callers that do
/// `getPermissions(pd1) == getPermissions(pd2)` see consistent identity.
///
/// Allocates outside the state lock for the same reason as
/// [`ensure_default_policy_object`] — see the note there.
fn ensure_shared_permission_collection(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(current) = security_slot(&*ctx, Slot::SharedPermissions) {
        return Ok(current);
    }
    let perms = build_permissive_collection(ctx)?;
    // Published with no allocation in between; from here on this module's
    // `VM_ROOT_SOURCES` row keeps the slot alive and current. The
    // AllPermission entry in slot 0 stays live via normal heap tracing from
    // this root.
    Ok(publish_security_slot(&*ctx, Slot::SharedPermissions, perms))
}

/// Build a synthetic `java.security.Permissions` collection seeded with a
/// single `java.security.AllPermission` entry. The collection's
/// `setReadOnly()` flag is set in slot 1 so accidental mutation is
/// observable; `Permissions.implies(Permission)` (the JDK bytecode body)
/// returns `true` for any permission once `AllPermission` is present.
///
/// SECURITY POSTURE: this helper returns a SHARED collection reused across
/// every call to `Policy.getPermissions(...)`. Do **not** mutate the
/// returned object after publication. The read-only flag enforces this at
/// the JDK API level — `PermissionCollection.add(...)` raises
/// `SecurityException` once `setReadOnly()` is true.
fn build_permissive_collection(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-e: the collection is rooted across the AllPermission's
    // allocation (which may collect and move it) and re-read after it.
    let mut scope = NativeHandleScope::new(ctx);
    let perms = try_alloc_concurrent_synthetic(&mut *scope, "java/security/Permissions", 2)?;
    let perms_h = scope.root(perms);
    let all_perm = try_alloc_concurrent_synthetic(&mut *scope, "java/security/AllPermission", 0)?;
    let perms: ObjectRef = scope.get(&perms_h);
    // Slot 0 = the AllPermission entry (we reuse the synthetic Permissions
    // 2-field layout: [allPermission, readOnly]).
    scope.set_field(perms, 0, Value::Object(Some(all_perm)));
    // Slot 1 = readOnly = true. `PermissionCollection.isReadOnly()` reads
    // this slot via `get_field_by_name("readOnly")`; the synthetic
    // Permissions field layout in `class_manager.rs` uses an int-backed
    // boolean (1 = read-only, 0 = mutable).
    scope.set_field(perms, 1, Value::Int(1));
    Ok(perms)
}

// ---------------------------------------------------------------------------
// Global policy (parsed java.policy or None = allow-all default)
// ---------------------------------------------------------------------------

static ACTIVE_POLICY: RwLock<Option<Policy>> = RwLock::new(None);

/// Install a policy parsed from a java.policy file. Pass `None` to revert to
/// the default allow-all behavior.
pub fn set_active_policy(policy: Option<Policy>) {
    let mut g = ACTIVE_POLICY.write().unwrap_or_else(|e| e.into_inner());
    *g = policy;
}

/// The path [`load_policy_file`] most recently loaded from, so
/// `java.security.Policy.refresh()` can genuinely re-read it. `None` when no
/// policy file was ever loaded (the allow-all default), in which case there is
/// nothing to reload.
static ACTIVE_POLICY_PATH: RwLock<Option<std::path::PathBuf>> = RwLock::new(None);

/// Load and install the policy from a file path.
pub fn load_policy_file<P: AsRef<Path>>(path: P) -> Result<(), PolicyError> {
    let path = path.as_ref().to_path_buf();
    let policy = Policy::from_file(&path)?;
    set_active_policy(Some(policy));
    {
        let mut slot = ACTIVE_POLICY_PATH
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *slot = Some(path);
    }
    Ok(())
}

/// Re-read and reinstall the policy from the file [`load_policy_file`] last
/// used. Returns `false` when no policy file is configured (nothing to
/// refresh) and leaves the active policy untouched if the re-read fails — the
/// JDK's `Policy.refresh()` likewise keeps the previous configuration when a
/// reload cannot be completed rather than falling open.
pub fn refresh_active_policy() -> bool {
    let path = ACTIVE_POLICY_PATH
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(path) = path else {
        return false;
    };
    match Policy::from_file(&path) {
        Ok(policy) => {
            set_active_policy(Some(policy));
            true
        }
        Err(_) => false,
    }
}

/// Query: is the given (permission_class, target, actions) granted under the
/// currently active policy? Returns `true` if either (a) no policy is installed
/// (default allow-all) or (b) any grant covers the request.
pub fn policy_allows(
    permission_class: &str,
    target: &str,
    actions: &str,
    code_base: Option<&str>,
) -> bool {
    policy_allows_full(permission_class, target, actions, code_base, &[])
}

/// Like [`policy_allows`] but also consults the calling frame's signer
/// certificate SHA-256 digests for `grant signedBy "..."` enforcement.
pub fn policy_allows_full(
    permission_class: &str,
    target: &str,
    actions: &str,
    code_base: Option<&str>,
    cert_digests: &[String],
) -> bool {
    policy_allows_full_generic(permission_class, target, actions, code_base, cert_digests)
}

/// Like [`policy_allows_full`] but generic over the digest slice element
/// type. The hot `checkPermission` path holds `&[Arc<str>]` borrowed
/// directly from the per-thread privileged-frame stack — going through
/// this variant skips the per-element `.to_string()` clone the
/// `&[String]` signature would otherwise force.
pub fn policy_allows_full_generic<S: AsRef<str>>(
    permission_class: &str,
    target: &str,
    actions: &str,
    code_base: Option<&str>,
    cert_digests: &[S],
) -> bool {
    let g = ACTIVE_POLICY.read().unwrap_or_else(|e| e.into_inner());
    match g.as_ref() {
        // SECURITY FIX (V10): no policy loaded = no enforcement (allow-all),
        // matching JDK behavior — UNLESS CRATONVM_REQUIRE_POLICY is set, in
        // which case a missing policy fails closed (deny). The certification
        // profile sets the flag; the default stays JDK-compatible.
        None => !require_policy_enabled(),
        Some(p) => p.implies_full(permission_class, target, actions, code_base, cert_digests),
    }
}

// ---------------------------------------------------------------------------
// Per-thread privileged-frame stack (for doPrivileged stack walks)
// ---------------------------------------------------------------------------

/// One entry on the per-thread privileged-frame stack.  Carries both the
/// codeBase URL (for `grant codeBase "..."` matching) and the SHA-256
/// digests of the JAR-signer blocks (for `grant signedBy "..."` matching).
///
/// Storage uses `Arc<str>` / `Arc<[Arc<str>]>` so the doPrivileged hot
/// path (~50k calls during JDK boot) reduces to a refcount bump on push
/// instead of cloning per-class signer-token strings on every frame.
/// The signer-token arc is interned per-ClassId in
/// [`SIGNER_TOKENS_CACHE`]; the codeBase fallback string is interned
/// per-ClassId in [`CLASS_CODE_BASE_CACHE`].
#[derive(Debug, Clone)]
struct PrivilegedFrame {
    code_base: Option<Arc<str>>,
    cert_digests: Arc<[Arc<str>]>,
}

thread_local! {
    /// When a permission check runs inside a `doPrivileged`, the top of
    /// this stack tells us which protection domain owns the invoking
    /// class.  That domain's codeBase + signer certs decide whether
    /// `grant codeBase "..."` / `grant signedBy "..."` clauses apply.
    static PRIVILEGED_STACK: RefCell<Vec<PrivilegedFrame>> = const { RefCell::new(Vec::new()) };
}

/// Internal: push a frame already shaped as `Arc<str>` / `Arc<[Arc<str>]>`.
/// The doPrivileged callees use this to avoid per-call allocation.
fn push_privileged_frame_arc(code_base: Option<Arc<str>>, cert_digests: Arc<[Arc<str>]>) {
    PRIVILEGED_STACK.with(|s| {
        s.borrow_mut().push(PrivilegedFrame {
            code_base,
            cert_digests,
        })
    });
}

/// Push a frame with just a codeBase URL (no signer digests).  Kept for
/// tests and any non-hot caller that doesn't have cert tokens to push.
fn push_privileged_frame(code_base: Option<String>) {
    push_privileged_frame_arc(code_base.map(Arc::from), Arc::from(Vec::<Arc<str>>::new()));
}

/// Push a frame from owned `Vec<String>` tokens. Used by tests; converts
/// to the Arc-backed shape before pushing.
fn push_privileged_frame_full(code_base: Option<String>, cert_digests: Vec<String>) {
    let arc_tokens: Vec<Arc<str>> = cert_digests.into_iter().map(Arc::from).collect();
    push_privileged_frame_arc(code_base.map(Arc::from), arc_tokens.into());
}

fn pop_privileged_frame() -> Option<PrivilegedFrame> {
    PRIVILEGED_STACK.with(|s| s.borrow_mut().pop())
}

/// Return the codeBase of the currently-active privileged frame, if any.
///
/// This allocates a fresh `String` from the frame's `Arc<str>` — convenient
/// for tests / debug code. The hot `checkPermission` path should call
/// [`current_privileged_code_base_arc`] instead to avoid the clone.
pub fn current_privileged_code_base() -> Option<String> {
    PRIVILEGED_STACK.with(|s| {
        s.borrow()
            .last()
            .and_then(|frame| frame.code_base.as_ref().map(|a| a.to_string()))
    })
}

/// Arc-returning variant of [`current_privileged_code_base`]. Cheap
/// (refcount bump) — used by the `checkPermission` hot path.
pub fn current_privileged_code_base_arc() -> Option<Arc<str>> {
    PRIVILEGED_STACK.with(|s| s.borrow().last().and_then(|frame| frame.code_base.clone()))
}

/// Return the signer-cert SHA-256 digests of the currently-active
/// privileged frame, or an empty vector if none.  Exposed so policy
/// checks can verify a `grant signedBy "..."` clause applies to the
/// code on the privileged frame.
///
/// Allocates a fresh `Vec<String>` per call — kept for tests / debug
/// callers. The `checkPermission` hot path uses
/// [`current_privileged_cert_digests_arc`] which returns the
/// `Arc<[Arc<str>]>` directly (a refcount bump, no element clones).
pub fn current_privileged_cert_digests() -> Vec<String> {
    PRIVILEGED_STACK
        .with(|s| {
            s.borrow()
                .last()
                .map(|frame| frame.cert_digests.iter().map(|a| a.to_string()).collect())
        })
        .unwrap_or_default()
}

/// Arc-returning variant of [`current_privileged_cert_digests`]. Returns
/// the per-thread privileged-frame digest array as a refcount-bumped
/// `Arc<[Arc<str>]>` — no per-element string allocation. Used by the
/// `checkPermission` hot path (~50k calls during JDK boot).
pub fn current_privileged_cert_digests_arc() -> Arc<[Arc<str>]> {
    PRIVILEGED_STACK.with(|s| {
        s.borrow()
            .last()
            .map(|frame| frame.cert_digests.clone())
            .unwrap_or_else(|| Arc::from(Vec::<Arc<str>>::new()))
    })
}

/// Depth of the privileged-frame stack on the current thread. Exposed for
/// testing stack-walk semantics.
pub fn privileged_stack_depth() -> usize {
    PRIVILEGED_STACK.with(|s| s.borrow().len())
}

// ---------------------------------------------------------------------------
// Permission checking (policy-aware)
// ---------------------------------------------------------------------------

/// Read the `name` field (field 0) from a Permission synthetic object and
/// return it as a Rust String, or empty if unavailable.
fn read_string_field(ctx: &mut dyn NativeContext, obj: ObjectRef, field_idx: usize) -> String {
    match ctx.get_field(obj, field_idx) {
        Value::Object(Some(sref)) => ctx.read_string(sref).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Construct and throw a real `java.security.AccessControlException`
/// (a subclass of `java.lang.SecurityException`) carrying `message`.
///
/// We build the genuine Java object rather than route through a
/// `RuntimeError` variant because the shared `RuntimeError::SecurityException`
/// maps to the bare `java.lang.SecurityException` class — JDK code that
/// does `catch (AccessControlException e)` (the conventional catch around
/// `checkPermission`) would miss a plain `SecurityException`. The
/// AccessControlException(String) constructor exists on JDK 25.
///
/// If the class cannot be built at all (e.g. it can't be resolved in a
/// stripped runtime), fall back to the `RuntimeError::SecurityException`
/// path so a denial is still surfaced as *some* SecurityException rather
/// than silently allowed.
///
/// Round 15 wave 4, lane jdkonly5 (R1 of
/// `r15w4-jdkonly4-builder-review-findings-CLOSED-20260930.md`): built by
/// `throwable_mint::construct_thrown`, so a constructor that THREW propagates
/// its own throwable (it was masked by the flat fallback), and the computed
/// denial text is a fresh string (it was interned into the never-pruned pool).
/// The arguments are rooted across `<init>` by the VM.
fn throw_access_control_exception(
    ctx: &mut dyn NativeContext,
    message: String,
) -> MethodCallFailed {
    let msg = ctx.create_string_uninterned(&message);
    crate::throwable_mint::construct_thrown(
        ctx,
        "java/security/AccessControlException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
        // Still a SecurityException, just the base class.
        || RuntimeError::SecurityException { message }.into(),
    )
}

/// Check a permission object. Enforcement only bites when the active
/// policy actually denies the request; with no policy loaded the result
/// is allow-all (the JDK default when a SecurityManager is installed
/// programmatically without a `java.policy`), so apps that never
/// configure a policy — including the BouncyCastle regression suite,
/// which installs neither a SecurityManager nor a policy — are never
/// affected.
///
/// If a policy is loaded, the grant list is consulted using the
/// currently-active privileged frame's code base (if any) to disambiguate
/// `codeBase "..."` grants. A denial throws
/// `java.security.AccessControlException`.
fn check_permission_impl(ctx: &mut dyn NativeContext, perm: ObjectRef) -> MethodCallResult {
    let class_id = ctx.class_id_of_object(perm);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();

    // Permission conventions: field 0 = name/target, field 1 = actions.
    let target = read_string_field(ctx, perm, 0);
    let actions = read_string_field(ctx, perm, 1);

    // Hot path: pull the per-thread privileged frame state as Arcs (a
    // refcount bump per access) instead of cloning a fresh
    // `Vec<String>` plus per-element `.to_string()`s on every
    // `checkPermission`. The Arc-migrated frame storage was added in
    // round 3 specifically to enable this.
    let code_base = current_privileged_code_base_arc();
    let cert_digests = current_privileged_cert_digests_arc();

    let allowed = policy_allows_full_generic(
        &class_name,
        &target,
        &actions,
        code_base.as_deref(),
        &cert_digests,
    );

    if allowed {
        tracing::trace!(
            permission_class = %class_name,
            target = %target,
            actions = %actions,
            code_base = ?code_base,
            "SecurityManager.checkPermission: ALLOW"
        );
        Ok(None)
    } else {
        tracing::debug!(
            permission_class = %class_name,
            target = %target,
            actions = %actions,
            code_base = ?code_base,
            "SecurityManager.checkPermission: DENY"
        );
        let message = access_denied_text(&class_name, &target, &actions, true);
        Err(throw_access_control_exception(ctx, message))
    }
}

/// Slot 0 of a synthetic permission built by [`sm_check_synthetic_permission`].
#[derive(Clone, Copy)]
enum SmPermissionName<'a> {
    /// The native's own argument (a path, host or property `String`).
    Arg(Value),
    /// A fresh `String` with this text.
    Text(&'a str),
}

/// gc-common w20-e: build the two-slot synthetic permission
/// `[name/target, actions]` of class `class` and run it through
/// [`check_permission_impl`]. The `SecurityManager.checkXxx` natives used to
/// do this inline, storing the argument and a fresh actions / name `String`
/// into the permission's address from before those strings' allocations
/// (and the argument's address from before the permission's). Everything is
/// rooted here and re-read after each allocation.
fn sm_check_synthetic_permission(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: SmPermissionName<'_>,
    actions: Option<&str>,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let arg_h = match name {
        SmPermissionName::Arg(Value::Object(Some(arg))) => Some(scope.root(arg)),
        _ => None,
    };
    let perm = try_alloc_concurrent_synthetic(&mut *scope, class, 2)?;
    let perm_h = scope.root(perm);
    let slot0 = match (name, &arg_h) {
        (SmPermissionName::Text(text), _) => Value::Object(Some(scope.create_string(text))),
        (SmPermissionName::Arg(_), Some(h)) => Value::Object(Some(scope.get(h))),
        (SmPermissionName::Arg(v), None) => v,
    };
    let perm: ObjectRef = scope.get(&perm_h);
    scope.set_field(perm, 0, slot0);
    let slot1 = match actions {
        Some(text) => Value::Object(Some(scope.create_string(text))),
        None => Value::Object(None),
    };
    let perm: ObjectRef = scope.get(&perm_h);
    scope.set_field(perm, 1, slot1);
    check_permission_impl(&mut *scope, perm)
}

/// Evaluate a Permission object against the currently-active parsed policy,
/// using the active privileged frame's code base / signer digests for
/// `codeBase "..."` / `signedBy "..."` matching. Returns `true` if the
/// permission is implied (or if no policy is loaded — the allow-all
/// default). This is the shared core used by both `SecurityManager`-style
/// `checkPermission` and the `Policy.implies(...)` native so the two stay
/// in lock-step.
fn policy_implies_permission(ctx: &mut dyn NativeContext, perm: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(perm);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    // Permission conventions: field 0 = name/target, field 1 = actions.
    let target = read_string_field(ctx, perm, 0);
    let actions = read_string_field(ctx, perm, 1);

    let code_base = current_privileged_code_base_arc();
    let cert_digests = current_privileged_cert_digests_arc();

    policy_allows_full_generic(
        &class_name,
        &target,
        &actions,
        code_base.as_deref(),
        &cert_digests,
    )
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_security_manager_natives(r: &mut NativeMethodRegistry) {
    register_security_manager(r);
    register_system_security(r);
    register_access_controller(r);
    register_access_control_context(r);
    register_policy_natives(r);
}

// ---------------------------------------------------------------------------
// java.lang.SecurityManager
// ---------------------------------------------------------------------------

fn register_security_manager(r: &mut NativeMethodRegistry) {
    let sm = "java/lang/SecurityManager";

    // <init>()V — STUB-REMOVAL (wave 3). This was a bare no-op justified as
    // "no fields to initialize". That is only half the ctor: real
    // `java.lang.SecurityManager()` also asks the CURRENTLY installed manager
    // for `RuntimePermission("createSecurityManager")` before a second one is
    // allowed to exist. CratonVM models an installable SecurityManager for
    // real — `get_security_manager` gates `Runtime.exec`/`ProcessBuilder.start`
    // (`lang_system::check_exec_or_throw`) and the Panama host-call path — so a
    // no-op here let code running under a restrictive manager mint its own
    // manager object unchallenged. Run the check through the same policy core
    // `checkPermission` uses, without needing a Permission instance.
    //
    // Nothing changes for the overwhelmingly common cases: no manager
    // installed (the first `new SecurityManager()`) short-circuits, and with no
    // `java.policy` loaded `policy_allows_full_generic` is allow-all, exactly
    // as before.
    r.register(sm, "<init>", "()V", |ctx, _args| {
        if get_security_manager(&*ctx).is_some() {
            let code_base = current_privileged_code_base_arc();
            let cert_digests = current_privileged_cert_digests_arc();
            if !policy_allows_full_generic(
                "java/lang/RuntimePermission",
                "createSecurityManager",
                "",
                code_base.as_deref(),
                &cert_digests,
            ) {
                return Err(throw_access_control_exception(
                    ctx,
                    access_denied_text(
                        "java/lang/RuntimePermission",
                        "createSecurityManager",
                        "",
                        false,
                    ),
                ));
            }
        }
        // Otherwise nothing to initialise: the object is a marker and the
        // global singleton is installed by `System.setSecurityManager`.
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // getRootGroup()Ljava/lang/ThreadGroup;
    //
    // JBoss Modules asks the SecurityManager for the root thread group during
    // Host Controller bootstrap. Real-JDK bytecode walks ThreadGroup parents
    // through the current Thread mirror, which is exactly the pre-bootstrap
    // layout window where CratonVM may still have only a synthetic thread
    // holder. Return a minimal "system" group directly so callers get a real
    // ThreadGroup object without depending on that fragile walk.
    r.register(
        sm,
        "getRootGroup",
        "()Ljava/lang/ThreadGroup;",
        |ctx, _args| {
            let group = try_alloc_concurrent_synthetic(ctx, "java/lang/ThreadGroup", 4)?;
            let pin_base = ctx.pin_native_root(group);
            let name = ctx.create_string("system");
            let group = ctx.read_native_pin(pin_base, group);
            let name = Value::Object(Some(name));

            // By NAME only. This used to write the same fields a second time by
            // raw index (0..3) under an `object_num_fields >= 4` guard, against
            // the legacy synthetic order — and that order is transposed twice
            // over against a real image, so the raw pass put the name `String`
            // in `parent`, nulled `name`, set `maxPriority` to 0 and `daemon`
            // to true. It ran AFTER the by-name writes, so it won: on a real
            // image `getRootGroup()` handed back a group whose `getName()` was
            // null and whose `getParent()` was a `String`.
            //
            // The guard it sat behind is the one this tree keeps re-learning:
            // `object_num_fields >= 4` stops an out-of-range write, not a
            // wrong-field one. A named write resolves on the receiver's own
            // class and is right on either layout, so the raw pass had nothing
            // to add even when its order was right.
            //
            // The `destroyed` write went with it: JDK 21+ `ThreadGroup` has no
            // such field and neither does the fabricated model, so
            // `set_field_by_name` was silently doing nothing and nothing reads
            // it back.
            ctx.set_field_by_name(group, "name", name);
            ctx.set_field_by_name(group, "parent", Value::Object(None));
            ctx.set_field_by_name(group, "maxPriority", Value::Int(10));
            ctx.set_field_by_name(group, "daemon", Value::Int(0));
            ctx.unpin_native_roots(pin_base);
            Ok(Some(Value::Object(Some(group))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // checkPermission(Permission)V
    r.register(
        sm,
        "checkPermission",
        "(Ljava/security/Permission;)V",
        |ctx, args| {
            let perm = obj_arg(args, 1)?;
            check_permission_impl(ctx, perm)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // checkRead(String)V — delegates to checkPermission with FilePermission("read")
    // (field 0 = path (the file string), field 1 = actions). gc-common w20-e:
    // every checkXxx below builds its permission through
    // `sm_check_synthetic_permission`, which roots it across the strings'
    // allocations.
    r.register(sm, "checkRead", "(Ljava/lang/String;)V", |ctx, args| {
        let _file = obj_arg(args, 1)?;
        let name = SmPermissionName::Arg(args[1]);
        sm_check_synthetic_permission(ctx, "java/io/FilePermission", name, Some("read"))
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkWrite(String)V — delegates to checkPermission with FilePermission("write")
    r.register(sm, "checkWrite", "(Ljava/lang/String;)V", |ctx, args| {
        let _file = obj_arg(args, 1)?;
        let name = SmPermissionName::Arg(args[1]);
        sm_check_synthetic_permission(ctx, "java/io/FilePermission", name, Some("write"))
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkConnect(String, int)V — delegates to checkPermission with SocketPermission
    r.register(sm, "checkConnect", "(Ljava/lang/String;I)V", |ctx, args| {
        let _host = obj_arg(args, 1)?;
        let name = SmPermissionName::Arg(args[1]); // host string
        sm_check_synthetic_permission(ctx, "java/net/SocketPermission", name, Some("connect"))
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkExec(String)V — delegates to checkPermission with FilePermission("execute")
    r.register(sm, "checkExec", "(Ljava/lang/String;)V", |ctx, args| {
        let _cmd = obj_arg(args, 1)?;
        let name = SmPermissionName::Arg(args[1]);
        sm_check_synthetic_permission(ctx, "java/io/FilePermission", name, Some("execute"))
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkDelete(String)V — delegates to checkPermission with FilePermission("delete")
    r.register(sm, "checkDelete", "(Ljava/lang/String;)V", |ctx, args| {
        let _file = obj_arg(args, 1)?;
        let name = SmPermissionName::Arg(args[1]);
        sm_check_synthetic_permission(ctx, "java/io/FilePermission", name, Some("delete"))
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkPropertyAccess(String)V — delegates to checkPermission with PropertyPermission
    r.register(
        sm,
        "checkPropertyAccess",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let _prop = obj_arg(args, 1)?;
            let name = SmPermissionName::Arg(args[1]); // property name
            sm_check_synthetic_permission(ctx, "java/util/PropertyPermission", name, Some("read"))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // checkAccess(Thread)V — delegates to checkPermission with
    // RuntimePermission("modifyThread") so a loaded policy can deny it.
    // SECURITY FIX (V10): previously hardcoded `Ok(None)` (always allow),
    // which meant a restrictive java.policy could never constrain thread
    // access. Route through check_permission_impl like the other checkXxx
    // handlers; under the default (no policy) the impl still allows.
    r.register(sm, "checkAccess", "(Ljava/lang/Thread;)V", |ctx, args| {
        let _thread = obj_arg(args, 1)?;
        let name = SmPermissionName::Text("modifyThread");
        sm_check_synthetic_permission(ctx, "java/lang/RuntimePermission", name, None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkAccess(ThreadGroup)V — delegates to checkPermission with
    // RuntimePermission("modifyThreadGroup") so a loaded policy can deny it.
    // SECURITY FIX (V10): previously hardcoded `Ok(None)` (always allow).
    r.register(
        sm,
        "checkAccess",
        "(Ljava/lang/ThreadGroup;)V",
        |ctx, args| {
            let _group = obj_arg(args, 1)?;
            let name = SmPermissionName::Text("modifyThreadGroup");
            sm_check_synthetic_permission(ctx, "java/lang/RuntimePermission", name, None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // checkCreateClassLoader()V — delegates to checkPermission with RuntimePermission
    r.register(sm, "checkCreateClassLoader", "()V", |ctx, args| {
        let _this = obj_arg(args, 0)?;
        let name = SmPermissionName::Text("createClassLoader");
        sm_check_synthetic_permission(ctx, "java/lang/RuntimePermission", name, None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // checkExit(int)V — delegates to checkPermission with RuntimePermission("exitVM.<code>")
    r.register(sm, "checkExit", "(I)V", |ctx, args| {
        let code = match args.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let text = format!("exitVM.{}", code);
        let name = SmPermissionName::Text(&text);
        sm_check_synthetic_permission(ctx, "java/lang/RuntimePermission", name, None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // getSecurityContext()Ljava/lang/Object; — returns a synthetic AccessControlContext
    r.register(
        sm,
        "getSecurityContext",
        "()Ljava/lang/Object;",
        |ctx, _args| {
            let acc = try_alloc_concurrent_synthetic(ctx, "java/security/AccessControlContext", 1)?;
            // field 0 = protection domains (null = no restriction)
            ctx.set_field(acc, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(acc))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// java.lang.System — getSecurityManager / setSecurityManager
// ---------------------------------------------------------------------------

fn register_system_security(r: &mut NativeMethodRegistry) {
    let sys = "java/lang/System";

    // getSecurityManager()Ljava/lang/SecurityManager;
    r.register(
        sys,
        "getSecurityManager",
        "()Ljava/lang/SecurityManager;",
        |ctx, _args| {
            let sm = get_security_manager(&*ctx);
            Ok(Some(Value::Object(sm)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // setSecurityManager(SecurityManager)V — STUB-REMOVAL (wave 3).
    //
    // This installed the new manager unconditionally. Real
    // `System.setSecurityManager` first asks the CURRENTLY installed manager
    // for `RuntimePermission("setSecurityManager")`, which is what stops code
    // running under a restrictive manager from simply replacing it with a
    // permissive one — or with `null`, disabling gating altogether.
    //
    // That mattered here rather than being cosmetic: CratonVM consults the
    // installed manager for real, gating `Runtime.exec`/`ProcessBuilder.start`
    // (`lang_system::check_exec_or_throw`) and the Panama host-call path. So
    // the missing check was a live sandbox escape, not a fidelity gap. It is
    // the other half of the `SecurityManager.<init>` gate above; the two are
    // only meaningful together, since gating construction while leaving
    // installation open just moves the bypass one call along.
    //
    // DECIDED (2026-07-29), deliberately NOT the JDK 24+ (JEP 486) behaviour of
    // throwing `UnsupportedOperationException` unconditionally.
    //
    // JEP 486 permanently disabled the SecurityManager on HotSpot, so a faithful
    // JDK 25 would refuse this call. CratonVM does not follow it, and the reason
    // is that here the manager is not decorative: `Runtime.exec` /
    // `ProcessBuilder.start` (`lang_system::check_exec_or_throw`) and the Panama
    // host-call path both ASK the installed manager before proceeding. Adopting
    // JEP 486 would make `System.setSecurityManager` throw and leave those two
    // gates permanently un-consulted — i.e. it would REMOVE enforcement in the
    // name of fidelity. Between "matches HotSpot's refusal" and "keeps the only
    // sandbox this VM has", the second wins.
    //
    // The cost is named rather than hidden: an application that expects JEP 486
    // semantics (catching `UnsupportedOperationException` to detect that
    // security managers are gone) sees an install succeed instead. Revisit only
    // together with a replacement for the exec/Panama gating — the two cannot
    // be separated, which is why this is a security-model decision and not a
    // stub to remove.
    //
    // Unchanged for the common cases: with no manager yet installed the check
    // short-circuits, and with no `java.policy` loaded
    // `policy_allows_full_generic` is allow-all.
    r.register(
        sys,
        "setSecurityManager",
        "(Ljava/lang/SecurityManager;)V",
        |ctx, args| {
            if get_security_manager(&*ctx).is_some() {
                let code_base = current_privileged_code_base_arc();
                let cert_digests = current_privileged_cert_digests_arc();
                if !policy_allows_full_generic(
                    "java/lang/RuntimePermission",
                    "setSecurityManager",
                    "",
                    code_base.as_deref(),
                    &cert_digests,
                ) {
                    return Err(throw_access_control_exception(
                        ctx,
                        access_denied_text(
                            "java/lang/RuntimePermission",
                            "setSecurityManager",
                            "",
                            false,
                        ),
                    ));
                }
            }
            let sm = match args.get(0) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            set_security_manager(ctx, sm);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// java.security.AccessController
// ---------------------------------------------------------------------------

/// Per-ClassId memoisation of [`action_code_base`].
///
/// The hot path is the synthetic-`class:<name>` fallback, which used to
/// `format!` a fresh `String` on every doPrivileged call.  JDK boot
/// hits doPrivileged ~50k times; caching the formatted `Arc<str>`
/// turns those allocations into a hashmap probe + refcount bump.
///
/// `None` is also cached (as a sentinel `OnceLock`-style miss) — see
/// the `Option<Arc<str>>` value type — so unknown classes don't keep
/// re-querying the class manager.  Cert digests don't change at runtime
/// so no invalidation is needed.
static CLASS_CODE_BASE_CACHE: OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<(usize, u32), Option<Arc<str>>>>,
> = OnceLock::new();

/// Per-ClassId memoisation of [`action_cert_digests`].
///
/// `class_code_source_cert_digests` returns an owned `Vec<String>` and
/// every `parse_signer_dn` call walks PKCS#7 DER on every invocation —
/// per-class data that's stable for the lifetime of the JVM.  Stored
/// as `Arc<[Arc<str>]>` so the doPrivileged push reduces to two
/// refcount bumps (`Arc::clone` on the slice header + Arc tokens reused
/// by-reference).
static SIGNER_TOKENS_CACHE: OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<(usize, u32), Arc<[Arc<str>]>>>,
> = OnceLock::new();

/// Cached form of [`action_code_base`] keyed by ClassId.  Returns the
/// same `Arc<str>` on every hit so the doPrivileged push is a refcount
/// bump.  See [`CLASS_CODE_BASE_CACHE`] for the rationale.
fn cached_action_code_base(ctx: &mut dyn NativeContext, class_id: ClassId) -> Option<Arc<str>> {
    let cache = CLASS_CODE_BASE_CACHE
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some(slot) = cache.read().get(&key) {
        return slot.clone();
    }
    // Slow path: derive the URL the same way `action_code_base` did.
    let computed: Option<Arc<str>> = if let Some(url) = ctx.class_code_base(class_id) {
        Some(Arc::from(url))
    } else {
        ctx.class_name_of_id(class_id)
            .map(|name| Arc::from(format!("class:{name}")))
    };
    // Insert (clone the Arc, not the str body).
    cache.write().insert(key, computed.clone());
    computed
}

/// Cached form of [`action_cert_digests`] keyed by ClassId.  The
/// PKCS#7 DER walk in [`x509::parse_signer_dn`] runs at most once per
/// class for the lifetime of the JVM; subsequent doPrivileged calls
/// reuse the same `Arc<[Arc<str>]>`.
fn cached_signer_tokens(ctx: &mut dyn NativeContext, class_id: ClassId) -> Arc<[Arc<str>]> {
    let cache = SIGNER_TOKENS_CACHE
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some(arc) = cache.read().get(&key).cloned() {
        return arc;
    }
    // Slow path: build the tokens via the original logic.
    let mut tokens: Vec<Arc<str>> = ctx
        .class_code_source_cert_digests(class_id)
        .into_iter()
        .map(Arc::from)
        .collect();
    for pkcs7 in ctx.class_code_source_certs(class_id) {
        if let Ok(dn) = x509::parse_signer_dn(&pkcs7) {
            tokens.push(Arc::from(dn));
        }
    }
    let arc: Arc<[Arc<str>]> = tokens.into();
    cache.write().insert(key, Arc::clone(&arc));
    arc
}

/// Derive the codeBase URL from the action object's declaring class.
///
/// Real HotSpot walks the stack and reads the `ProtectionDomain` of the
/// frame that invoked `doPrivileged`; the `ProtectionDomain` carries a
/// `CodeSource` whose `location` is a real URL (typically `file:/.../x.jar`).
/// We emulate that by looking up the action class's attached `CodeSource`
/// in the class manager — if present, its URL is returned directly so
/// grants like `grant codeBase "file:/opt/app.jar" { ... }` match without
/// further translation.
///
/// When no real URL is available (synthetic / stub classes, JDK internals
/// from a jimage), we fall back to the synthetic `class:` URI so tests
/// and legacy policy entries that use `class:com/acme/-` still work.
///
/// Tests still call this directly — production doPrivileged uses
/// [`cached_action_code_base`] which is hot-path-optimised.
fn action_code_base(ctx: &mut dyn NativeContext, action: ObjectRef) -> Option<String> {
    let cid = ctx.class_id_of_object(action);
    cached_action_code_base(ctx, cid).map(|a| a.to_string())
}

/// Return a mixed slice of signer-identifier tokens for the action
/// object's class:
///
/// * SHA-256 hex digests (64 chars) — one per signer block;
/// * Canonical RFC 4514 DN strings (one per parsable signer cert,
///   prefixed with a type like `CN=…`) returned by
///   [`x509::parse_signer_dn`].
///
/// The policy engine's `signed_by_matches` distinguishes the two
/// formats by tag-shape: 64 hex chars → digest; contains `=` → DN.
/// Empty when the class is unsigned or has no CodeSource.
///
/// Production callers go through [`cached_signer_tokens`]; this helper
/// remains for any non-hot caller that wants the `Vec<String>` shape.
#[allow(dead_code)]
fn action_cert_digests(ctx: &mut dyn NativeContext, action: ObjectRef) -> Vec<String> {
    let cid = ctx.class_id_of_object(action);
    cached_signer_tokens(ctx, cid)
        .iter()
        .map(|a| a.to_string())
        .collect()
}

fn register_access_controller(r: &mut NativeMethodRegistry) {
    let ac = "java/security/AccessController";

    // doPrivileged(PrivilegedAction)Ljava/lang/Object;
    // Invokes action.run() and returns the result. Pushes a privileged frame
    // whose codeBase is derived from the action object's class name so that
    // permission checks inside action.run() see the *action's* code source,
    // not the caller's.
    r.register(
        ac,
        "doPrivileged",
        "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
        // Interpreter round i6 wave 120, lane L3 (stage 2 of i46-L3): a table
        // row (`lang_reflect::ReflectiveEntryRow`), see `DoPrivilegedRow`.
        crate::lang_reflect::reflective_entry::<DoPrivilegedRow>, cratonvm_native_api::NativeKind::Bridge,
    );

    // doPrivileged(PrivilegedExceptionAction)Ljava/lang/Object;
    // Invokes action.run() which may throw a checked exception.
    r.register(
        ac,
        "doPrivileged",
        "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;",
        // Wave 120: see `DoPrivilegedExceptionRow`.
        crate::lang_reflect::reflective_entry::<DoPrivilegedExceptionRow>, cratonvm_native_api::NativeKind::Bridge,
    );

    // doPrivileged(PrivilegedAction, AccessControlContext)Ljava/lang/Object;
    // The AccessControlContext caps the privileges; for our model we respect
    // it by pushing the *action's* code base as the privileged frame.
    r.register(
        ac,
        "doPrivileged",
        "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
        // Wave 120: see `DoPrivilegedInContextRow`.
        crate::lang_reflect::reflective_entry::<DoPrivilegedInContextRow>, cratonvm_native_api::NativeKind::Bridge,
    );

    // getContext()Ljava/security/AccessControlContext;
    // Returns a synthetic AccessControlContext with no protection-domain restrictions.
    // Not over a real JDK 25 under `CRATONVM_ACCESS_CONTROLLER_JDK_BODIES`
    // (see `access_controller_jdk_bodies`): the JDK's body returns its one
    // static `NO_PERMISSIONS_ACC`, where this answered a fresh object per call.
    if !access_controller_jdk_bodies(r) {
        r.register(
            ac,
            "getContext",
            "()Ljava/security/AccessControlContext;",
            |ctx, _args| {
                let acc =
                    try_alloc_concurrent_synthetic(ctx, "java/security/AccessControlContext", 1)?;
                ctx.set_field(acc, 0, Value::Object(None)); // no protection domains
                Ok(Some(Value::Object(Some(acc))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // getStackAccessControlContext()Ljava/security/AccessControlContext;
    // Spec: "Returns the AccessControl context of the calling thread's stack,
    // or null if no controls apply." Our VM doesn't enforce stack-based checks
    // by default (SecurityManager is deprecated-for-removal but still callable
    // per T8). Returning null is the canonical "no restrictions apply" answer
    // and matches JDK 25's behavior when System.getSecurityManager() returns
    // null — HotSpot itself short-circuits this to null in the no-SM path.
    r.register(
        ac,
        "getStackAccessControlContext",
        "()Ljava/security/AccessControlContext;",
        native_ac_get_stack_access_control_context, cratonvm_native_api::NativeKind::Bridge,
    );

    // getInheritedAccessControlContext()Ljava/security/AccessControlContext;
    // Spec: returns the context inherited from the thread-creator's point. We
    // don't track inherited ACs (since we don't enforce stack-based checks) —
    // null is the canonical "no inherited context" answer and is symmetric
    // with `getStackAccessControlContext` above.
    r.register(
        ac,
        "getInheritedAccessControlContext",
        "()Ljava/security/AccessControlContext;",
        native_ac_get_inherited_access_control_context, cratonvm_native_api::NativeKind::Bridge,
    );

    // getProtectionDomain(Class)Ljava/security/ProtectionDomain;
    // Delegates to Class.getProtectionDomain0 when available (landed by the
    // concurrent Class-natives agent). On the "not-yet-landed" transient case,
    // the invoke fails with UnsatisfiedLinkError; we swallow that and fall
    // back to returning null, which matches the pre-JDK-17 behaviour for
    // classes without an attached ProtectionDomain and is what AccessController
    // callers are defensively coded to handle.
    r.register(
        ac,
        "getProtectionDomain",
        "(Ljava/lang/Class;)Ljava/security/ProtectionDomain;",
        native_ac_get_protection_domain, cratonvm_native_api::NativeKind::Bridge,
    );

    // ensureMaterializedForStackWalk(Object)V
    // Spec (JDK 25 javadoc): "Ensures that the given object is materialized
    // when a stack-walk operation is subsequently performed." Used internally
    // by StackWalker to keep referenced objects alive across native frame
    // traversal. Rust's ownership model keeps objects live as long as JVM
    // references to them exist; no additional materialization step is needed.
    // Genuine spec no-op — intentionally ignores its argument.
    r.register(
        ac,
        "ensureMaterializedForStackWalk",
        "(Ljava/lang/Object;)V",
        native_ac_ensure_materialized_for_stack_walk, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// AccessController native bodies (T19 · N3 wave)
// ---------------------------------------------------------------------------
//
// These four natives are separated out as free functions (instead of inline
// closures) so unit tests can reference them directly and so the rationale
// comments have room to breathe. They are NOT silent stubs — each carries a
// spec-derived justification for the value it returns.

/// `AccessController.getStackAccessControlContext()` — no SecurityManager
/// semantics: return null. See registration for the full rationale.
fn native_ac_get_stack_access_control_context(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `AccessController.getInheritedAccessControlContext()` — no inherited-AC
/// tracking: return null. Symmetric with `getStackAccessControlContext`.
fn native_ac_get_inherited_access_control_context(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `AccessController.getProtectionDomain(Class)` — delegate to the Class
/// native that Agent N1 is landing in parallel. If that native is not yet
/// registered, the `invoke` below fails (UnsatisfiedLinkError-shaped); we
/// swallow the error and return null so callers see the documented "no PD
/// available" behaviour rather than a linker exception propagating up.
fn native_ac_get_protection_domain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let class_ref = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        // Null or absent class argument → null PD (caller contract).
        _ => return Ok(Some(Value::Object(None))),
    };
    // Delegate to Class.getProtectionDomain0 (landing via the parallel Class
    // natives agent). If the native isn't registered yet, `invoke` returns
    // an Err; treat that as "no PD available" and return null rather than
    // propagating the error to the AccessController caller.
    match ctx.invoke(
        "java/lang/Class",
        "getProtectionDomain0",
        "()Ljava/security/ProtectionDomain;",
        &[Value::Object(Some(class_ref))],
    ) {
        Ok(Some(v)) => Ok(Some(v)),
        // Delegate returned void/None — normalise to a null PD object.
        Ok(None) => Ok(Some(Value::Object(None))),
        // Delegate not yet registered (N1 hasn't landed) or raised: swallow
        // and return null. AccessController callers don't expect ULE here.
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

/// `AccessController.ensureMaterializedForStackWalk(Object)` — genuine spec
/// no-op. Rust GC keeps objects live as long as references to them exist, so
/// there is nothing to materialise.
fn native_ac_ensure_materialized_for_stack_walk(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// java.security.AccessControlContext
// ---------------------------------------------------------------------------

/// Over a real JDK 25, do `AccessController.getContext` and
/// `AccessControlContext.checkPermission` run the JDK's own bodies (JEP 486:
/// one static `NO_PERMISSIONS_ACC`, and `AccessControlException("checking
/// permissions is not supported")` from every `checkPermission`), as on
/// HotSpot in both modes? The registered stand-ins answered a fresh context
/// per call and this VM's policy emulation (allow-all without a policy).
/// They are kept where the emulation is the point -- the
/// `CRATONVM_REQUIRE_POLICY` / `CRATONVM_UNTRUSTED_CODE` profile, and the
/// synthetic class library -- and wherever
/// `CRATONVM_ACCESS_CONTROLLER_JDK_BODIES` is off. Read once per registry
/// (per VM), at registration. The `doPrivileged` bridges are unaffected.
/// Interpreter round i1 wave 50, lane L5
/// (`interpreter-L5-registered-accesscontroller-natives-diverge-from-jdk-25-FIXED-20261014`; probe
/// `L3W49AccessControllerSurface.java`).
fn access_controller_jdk_bodies(r: &NativeMethodRegistry) -> bool {
    r.real_jdk()
        && !require_policy_enabled()
}

/// `AccessController.doPrivileged(PrivilegedAction)`'s row (interpreter round
/// i6 wave 120, lane L3; stage 2 of i46-L3): the record of the
/// `doPrivileged` frame HotSpot lists below the action
/// (`stackwalker::PRIVILEGED_STEPS`; interpreter round i1 wave 49, lane L3)
/// now brackets the whole native, as its registration names it
/// ([`crate::lang_reflect::reflective_entry`]); the native pushes no Java
/// frame before the action's `run()`, so the recorded depths are the same.
/// Invokes `action.run()` and returns the result. Pushes a privileged frame
/// whose codeBase is derived from the action object's class name so that
/// permission checks inside `action.run()` see the *action's* code source,
/// not the caller's.
struct DoPrivilegedRow;

impl crate::lang_reflect::ReflectiveEntryRow for DoPrivilegedRow {
    const KIND: cratonvm_native_api::ReflectiveCallKind =
        cratonvm_native_api::ReflectiveCallKind::DoPrivileged;

    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let action = obj_arg(args, 0)?;
        // The cache slow paths below can cooperate with a moving GC before
        // `invoke_virtual` installs the Java `run()` receiver. Keep this
        // freshly allocated action rooted for the whole privileged window
        // and reread it at the dispatch boundary. JAXB exposed the stale
        // form with `ReflectionNavigator$10`: its captured superclass
        // search fields became unrelated live objects and `run()` repeated
        // forever.
        let action_pin = ctx.pin_native_root(action);
        let action_cur = ctx.read_native_pin(action_pin, action);
        // Single class_id_of_object lookup, then everything is keyed by
        // the cached ClassId — no per-call format!() or PKCS#7 walk.
        let cid = ctx.class_id_of_object(action_cur);
        let cb = cached_action_code_base(ctx, cid);
        let digests = cached_signer_tokens(ctx, cid);
        let dbg = dbg_dopriv_enabled();
        if dbg {
            let cls = ctx.class_name_of_id(cid).unwrap_or_else(|| "?".to_string());
            eprintln!("[doPriv] ENTER action class={cls}");
        }
        push_privileged_frame_arc(cb, digests);
        let action_cur = ctx.read_native_pin(action_pin, action);
        let result = ctx.invoke_virtual(action_cur, "run", "()Ljava/lang/Object;", &[]);
        pop_privileged_frame();
        ctx.unpin_native_roots(action_pin);
        if dbg {
            let cls = ctx.class_name_of_id(cid).unwrap_or_else(|| "?".to_string());
            eprintln!("[doPriv] EXIT action class={cls} ok={}", result.is_ok());
        }
        result
    }
}

/// `doPrivileged(PrivilegedExceptionAction)`'s row (wave 120): the record
/// (the `doPrivileged` frame) is kept through the wrap below, so the
/// wrapper's trace starts at the JDK's `wrapException` above it, as
/// HotSpot's does (wave 49, lane L3).
struct DoPrivilegedExceptionRow;

impl crate::lang_reflect::ReflectiveEntryRow for DoPrivilegedExceptionRow {
    const KIND: cratonvm_native_api::ReflectiveCallKind =
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedException;

    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let action = obj_arg(args, 0)?;
        let action_pin = ctx.pin_native_root(action);
        let action_cur = ctx.read_native_pin(action_pin, action);
        let cid = ctx.class_id_of_object(action_cur);
        let cb = cached_action_code_base(ctx, cid);
        let digests = cached_signer_tokens(ctx, cid);
        push_privileged_frame_arc(cb, digests);
        let action_cur = ctx.read_native_pin(action_pin, action);
        let result = ctx.invoke_virtual(action_cur, "run", "()Ljava/lang/Object;", &[]);
        pop_privileged_frame();
        ctx.unpin_native_roots(action_pin);
        // A CHECKED exception comes back wrapped; an unchecked one does
        // not. That wrapping is the entire reason this overload exists
        // beside the `PrivilegedAction` one, and `javac` makes every caller
        // catch `PrivilegedActionException` -- so propagating the checked
        // exception raw threw it straight past a handler the compiler had
        // forced them to write:
        //
        //   doPrivileged((PrivilegedExceptionAction<String>) () -> {
        //       throw new IOException("checked"); })
        //     HotSpot  PrivilegedActionException wrapping java.io.IOException
        //     was      java.io.IOException
        //
        // MEASURED by `SecuritySurfaceSweep.java`.
        wrap_checked_in_privileged_action_exception(ctx, result)
    }
}

/// `doPrivileged(PrivilegedAction, AccessControlContext)`'s row (wave 120).
/// The AccessControlContext caps the privileges; for our model we respect it
/// by pushing the *action's* code base as the privileged frame.
struct DoPrivilegedInContextRow;

impl crate::lang_reflect::ReflectiveEntryRow for DoPrivilegedInContextRow {
    const KIND: cratonvm_native_api::ReflectiveCallKind =
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedInContext;

    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let action = obj_arg(args, 0)?;
        let action_pin = ctx.pin_native_root(action);
        let action_cur = ctx.read_native_pin(action_pin, action);
        let cid = ctx.class_id_of_object(action_cur);
        let cb = cached_action_code_base(ctx, cid);
        let digests = cached_signer_tokens(ctx, cid);
        push_privileged_frame_arc(cb, digests);
        let action_cur = ctx.read_native_pin(action_pin, action);
        let result = ctx.invoke_virtual(action_cur, "run", "()Ljava/lang/Object;", &[]);
        pop_privileged_frame();
        ctx.unpin_native_roots(action_pin);
        result
    }
}

fn register_access_control_context(r: &mut NativeMethodRegistry) {
    let acc = "java/security/AccessControlContext";

    // Not over a real JDK 25 under `CRATONVM_ACCESS_CONTROLLER_JDK_BODIES`
    // (see `access_controller_jdk_bodies`).
    if access_controller_jdk_bodies(r) {
        return;
    }
    // checkPermission(Permission)V — allow all under default policy
    r.register(
        acc,
        "checkPermission",
        "(Ljava/security/Permission;)V",
        |ctx, args| {
            let perm = obj_arg(args, 1)?;
            check_permission_impl(ctx, perm)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// T19_H9_ANCHOR_POLICY_NATIVES
// java.security.Policy
// ---------------------------------------------------------------------------
//
// Real JDK 25 throws `UnsupportedOperationException("Setting a system-wide
// Policy object is not supported")` from `Policy.setPolicy(Policy)` because
// JEP 411 is sealing the SecurityManager surface in JDK 25. JBoss Modules,
// WildFly, and many older libraries call `Policy.setPolicy(...)` during
// boot — KC16's `Main.main` does it via `Module.<clinit>` →
// `ModulesPolicy.install`. Throwing kills boot.
//
// cratonvm's lenient model accepts the installation: we store the reference
// in a process-wide singleton and answer subsequent queries from it.
//
// ENFORCEMENT: `implies(...)` now delegates to the parsed `java.policy`
// grant evaluation (`policy_implies_permission` → `policy_allows_full_generic`).
// When a Rust-side `Policy` (the parsed `java.policy` flavour) is configured,
// a permission not covered by any grant returns `false`; the
// `SecurityManager.checkPermission` flow above shares the same evaluation
// and throws `java.security.AccessControlException` on denial.
//
// SECURITY POSTURE: enforcement is policy-gated, not always-on. When no
// `java.policy` is configured the evaluation short-circuits to allow-all
// (the JDK default for a programmatically-installed SecurityManager with no
// policy), so apps that install neither a SecurityManager nor a policy —
// including the BouncyCastle regression suite — are entirely unaffected.
// Enforcement only ever DENIES once a policy has been parsed and installed
// via `set_active_policy` / `load_policy_file`.
fn register_policy_natives(r: &mut NativeMethodRegistry) {
    let p = "java/security/Policy";

    // getPolicy()Ljava/security/Policy; — process-wide singleton.
    // First call lazily allocates a synthetic `Policy` instance that
    // permits all (`AllPermission`-style). The lazy init runs under the
    // singleton mutex so the default is never observed half-initialised.
    r.register(p, "getPolicy", "()Ljava/security/Policy;", |ctx, _args| {
        if let Some(existing) = get_policy_object(&*ctx) {
            return Ok(Some(Value::Object(Some(existing))));
        }
        let p = ensure_default_policy_object(ctx)?;
        Ok(Some(Value::Object(Some(p))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // getPolicyNoCheck()Ljava/security/Policy; — internal JDK entry that
    // skips the SecurityManager check; for our lenient model this is
    // identical to `getPolicy`.
    r.register(
        p,
        "getPolicyNoCheck",
        "()Ljava/security/Policy;",
        |ctx, _args| {
            if let Some(existing) = get_policy_object(&*ctx) {
                return Ok(Some(Value::Object(Some(existing))));
            }
            let p = ensure_default_policy_object(ctx)?;
            Ok(Some(Value::Object(Some(p))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // isSet()Z — returns true if a Policy has been explicitly installed.
    // We treat the default-singleton path as "set" once it has been
    // materialised, matching `Policy.policyInfo.initialized` semantics in
    // real JDK after the first `getPolicy()` returns.
    r.register(p, "isSet", "()Z", |ctx, _args| {
        // Presence-only probe — no object address is dereferenced, so no
        // var-handle-root re-read is needed here. `ctx` is still required: the
        // answer is about THIS VM's Policy slot, not the process's.
        let set = policy_object_installed(&*ctx);
        Ok(Some(Value::Int(if set { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);

    // setPolicy(Policy)V — store `newPolicy` in the singleton slot. Must
    // NOT throw under any circumstance (T19.H9 contract). `null` clears
    // the slot; the next `getPolicy()` lazily falls back to the default.
    r.register(p, "setPolicy", "(Ljava/security/Policy;)V", |ctx, args| {
        let new_policy = match args.get(0) {
            Some(Value::Object(Some(o))) => Some(*o),
            _ => None, // null or missing arg
        };
        set_policy_object(ctx, new_policy);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // getPermissions(Ljava/security/ProtectionDomain;)Ljava/security/PermissionCollection;
    // Returns the SHARED permissive collection. Callers must not mutate.
    // The collection is read-only (slot 1 = 1) so a defensively-coded
    // caller that calls `add(...)` will see the JDK's standard
    // SecurityException without us doing anything special.
    r.register(
        p,
        "getPermissions",
        "(Ljava/security/ProtectionDomain;)Ljava/security/PermissionCollection;",
        |ctx, _args| {
            // Always return the process-wide shared collection — the
            // lenient model has no per-PD differentiation, and a stable
            // reference lets callers that do reference-equality checks
            // across calls (rare but legal) see consistent identity.
            let perms = ensure_shared_permission_collection(ctx)?;
            Ok(Some(Value::Object(Some(perms))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getPermissions(Ljava/security/CodeSource;)Ljava/security/PermissionCollection;
    // Same shared collection — code-source-based grants are not enforced.
    r.register(
        p,
        "getPermissions",
        "(Ljava/security/CodeSource;)Ljava/security/PermissionCollection;",
        |ctx, _args| {
            let perms = ensure_shared_permission_collection(ctx)?;
            Ok(Some(Value::Object(Some(perms))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // implies(ProtectionDomain, Permission)Z — delegate to the parsed
    // `java.policy` grant evaluation. With no Rust-side policy installed
    // this still returns `true` (allow-all default, matching the prior
    // contract and the no-SecurityManager BouncyCastle path); once a
    // policy IS configured, the answer reflects whether any grant covers
    // the requested permission.
    //
    // The Permission is the final argument: under the real VM the args are
    // `[this(Policy), protectionDomain, permission]`; some internal/test
    // call paths omit the receiver, leaving `[protectionDomain, permission]`.
    // In both shapes the Permission is `args.last()`, so we read it from
    // there rather than a fixed index.
    r.register(
        p,
        "implies",
        "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
        |ctx, args| {
            let perm = match args.last() {
                Some(Value::Object(Some(o))) => *o,
                // SECURITY FIX (V10): a null permission argument must DENY,
                // not allow. Returning allow for a null permission means a
                // caller asking "do I have <null>?" is told yes, which is
                // an unconditional bypass. A null permission carries no
                // grant to satisfy, so deny it.
                _ => return Ok(Some(Value::Int(0))),
            };
            let allowed = policy_implies_permission(ctx, perm);
            Ok(Some(Value::Int(if allowed { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // refresh()V — STUB-REMOVAL (wave 2): this claimed "cratonvm has no Policy
    // provider to reload", which stopped being true once `load_policy_file`
    // started parsing a real `java.policy` and `implies(...)` started
    // enforcing it. A no-op `refresh()` means an operator who edits the policy
    // file and calls `Policy.getPolicy().refresh()` keeps running under the
    // OLD grants with no indication — silently stale authorization. Re-read
    // the file that was actually loaded; a VM with no policy file configured
    // (the allow-all default) genuinely has nothing to reload, and a failed
    // re-read keeps the previous policy rather than falling open.
    r.register(p, "refresh", "()V", |_ctx, _args| {
        refresh_active_policy();
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // <init>()V — KEEP (genuinely empty): Policy is abstract in real JDK so
    // `new Policy()` would never compile, but JBoss Modules subclasses it and
    // the subclass ctor invokes `super.<init>()`. Real
    // `java.security.Policy()`'s body IS empty (the class holds only static
    // state), so an empty native is exact, not a stub — it exists purely so
    // the invokespecial path doesn't fall through to the missing-method
    // branch.
    r.register(p, "<init>", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// `PrivilegedActionException`-wrap a checked exception from a
/// `PrivilegedExceptionAction`, leaving everything else alone.
///
/// "Checked" is the JDK's own test in `AccessController.doPrivileged`: a
/// `RuntimeException` or an `Error` passes through, anything else is wrapped.
/// The two are asked by walking the thrown object's superclass chain, which is
/// the only classification available at this boundary -- and is the same test
/// the compiler applies.
///
/// A failure to CONSTRUCT the wrapper leaves the original exception in place:
/// losing the caller's exception in order to report a VM-internal problem with
/// the wrapper would be strictly worse than not wrapping.
fn wrap_checked_in_privileged_action_exception(
    ctx: &mut dyn NativeContext,
    result: MethodCallResult,
) -> MethodCallResult {
    let Err(MethodCallFailed::ExceptionThrown(thrown)) = result else {
        return result;
    };
    if throwable_is_unchecked(ctx, thrown) {
        return Err(MethodCallFailed::ExceptionThrown(thrown));
    }
    let pin = ctx.pin_native_root(thrown);
    let thrown_now = ctx.read_native_pin(pin, thrown);
    let built = cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/security/PrivilegedActionException",
        "(Ljava/lang/Exception;)V",
        &[Value::Object(Some(thrown_now))],
    );
    let thrown_now = ctx.read_native_pin(pin, thrown_now);
    ctx.unpin_native_roots(pin);
    match built {
        Ok(Some(wrapper)) => Err(MethodCallFailed::ExceptionThrown(wrapper)),
        Err(thrown) => Err(thrown),
        Ok(None) => Err(MethodCallFailed::ExceptionThrown(thrown_now)),
    }
}

/// Is `thrown` a `RuntimeException` or an `Error` -- i.e. one of the two
/// families `doPrivileged` passes through unwrapped?
fn throwable_is_unchecked(ctx: &dyn NativeContext, thrown: ObjectRef) -> bool {
    let mut cls = Some(ctx.class_id_of_object(thrown));
    let mut depth = 0;
    while let Some(c) = cls {
        depth += 1;
        if depth > 64 {
            break;
        }
        match ctx.class_name_of_id(c).as_deref() {
            Some("java/lang/RuntimeException") | Some("java/lang/Error") => return true,
            // Reaching `Throwable` without meeting either means checked.
            Some("java/lang/Throwable") | Some("java/lang/Object") => return false,
            _ => {}
        }
        cls = ctx.superclass_of(c);
    }
    // No verdict: treat as unchecked, which preserves the pre-2026-08-29
    // behaviour rather than inventing a wrapper for something unclassifiable.
    true
}
