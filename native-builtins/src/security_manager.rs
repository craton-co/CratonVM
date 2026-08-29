// SPDX-License-Identifier: Apache-2.0
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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
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
pub(crate) fn check_host_native_access_or_throw(
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
            format!("access denied (\"java/lang/RuntimePermission\" \"{permission_target}\")"),
        ))
    }
}

// ---------------------------------------------------------------------------
// Per-VM SecurityManager / Policy state
// ---------------------------------------------------------------------------

/// The heap objects this module owns, for ONE VM.
///
/// Every slot is `(identity_key, ObjectRef)`: the `ObjectRef` is the address as
/// last known to this table, and the identity key (stable across moves) lets a
/// read re-fetch the current address from the owning VM's var-handle-root
/// registry. Both halves are per-VM — that is the whole point of the struct,
/// see [`SECURITY_STATE`].
#[derive(Clone, Copy, Default)]
struct VmSecurityState {
    /// The `java.lang.SecurityManager` installed by `System.setSecurityManager`.
    security_manager: Option<(i32, ObjectRef)>,
    /// The `java.security.Policy` installed by `Policy.setPolicy`, or the
    /// lazily-materialised synthetic default.
    policy_object: Option<(i32, ObjectRef)>,
    /// The shared read-only `Permissions` collection handed out by
    /// `Policy.getPermissions(...)`.
    shared_permissions: Option<(i32, ObjectRef)>,
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
    fn slot(&self, slot: Slot) -> Option<(i32, ObjectRef)> {
        match slot {
            Slot::SecurityManager => self.security_manager,
            Slot::PolicyObject => self.policy_object,
            Slot::SharedPermissions => self.shared_permissions,
        }
    }

    fn set_slot(&mut self, slot: Slot, value: Option<(i32, ObjectRef)>) {
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
/// making the cached copy itself collector-visible AND rewritable.
static SECURITY_STATE: OnceLock<Mutex<std::collections::HashMap<usize, VmSecurityState>>> =
    OnceLock::new();

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
    f: impl FnOnce(&mut std::collections::HashMap<usize, VmSecurityState>) -> R,
) -> R {
    let mut guard = security_state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Read one slot of the CALLING VM's state. Another VM's slot is not
/// addressable from here — the key comes from `ctx`, never from a caller.
fn security_slot(ctx: &dyn NativeContext, slot: Slot) -> Option<(i32, ObjectRef)> {
    let vm = ctx.vm_identity();
    with_security_state(|table| table.get(&vm).and_then(|state| state.slot(slot)))
}

/// Store (or clear) one slot of the CALLING VM's state. Clearing the last
/// occupied slot drops the VM's row entirely.
fn set_security_slot(ctx: &dyn NativeContext, slot: Slot, entry: Option<(i32, ObjectRef)>) {
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
fn publish_security_slot(
    ctx: &dyn NativeContext,
    slot: Slot,
    entry: (i32, ObjectRef),
) -> (i32, ObjectRef) {
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

/// Re-read the CURRENT address of a cached `(identity_key, ObjectRef)` pair.
///
/// The collector now repoints the cached copy directly (see
/// [`gc_update_security_manager_refs`]), but the var-handle-root registry is
/// still consulted first so the two agree; contexts without a registry (mocks)
/// fall back to the cached ref.
fn resolve_slot(ctx: &dyn NativeContext, slot: (i32, ObjectRef)) -> ObjectRef {
    let (key, cached) = slot;
    ctx.read_var_handle_root(key).unwrap_or(cached)
}

/// Root `obj` in the calling VM and return the `(identity_key, ObjectRef)` pair
/// to cache. The key MUST be computed on the same address that was registered,
/// with no allocating call in between.
fn root_for_cache(ctx: &mut dyn NativeContext, obj: ObjectRef) -> (i32, ObjectRef) {
    ctx.register_var_handle_root(obj);
    (ctx.identity_hash_code(obj), obj)
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
            if let Some((_, obj)) = slot {
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
            if let Some((_, obj_ref)) = slot {
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
pub(crate) fn security_state_test_lock() -> std::sync::MutexGuard<'static, ()> {
    SECURITY_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Return the `java.lang.SecurityManager` installed in the CALLING VM, or
/// `None` if `System.setSecurityManager(null)` is in effect there (the
/// default).
///
/// `pub(crate)` so security-sensitive native entry points can consult the same
/// slot: `ProcessBuilder.start` / `Runtime.exec*` via
/// `lang_system::check_exec_or_throw`, and the Panama host-call gate via
/// `panama::check_native_access`.
///
/// The answer is scoped to `ctx`: another VM's manager is neither readable nor
/// clearable from here, so one VM can no longer disarm another's gates.
pub(crate) fn get_security_manager(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let slot = security_slot(ctx, Slot::SecurityManager)?;
    // Re-read the CURRENT address (see `resolve_slot`).
    Some(resolve_slot(ctx, slot))
}

fn set_security_manager(ctx: &mut dyn NativeContext, sm: Option<ObjectRef>) {
    // Keep alive + registry-remapped across GC moves (VarHandle-root pattern);
    // the identity key lets every later read re-read the current address.
    let entry = sm.map(|obj| root_for_cache(ctx, obj));
    set_security_slot(&*ctx, Slot::SecurityManager, entry);
}

/// Override the calling VM's SecurityManager slot for tests. Lets unit tests
/// install a synthetic SM object so they can exercise the checkExec
/// gating path without going through `System.setSecurityManager`.
///
/// Returns the previous value so callers can restore it on tear-down.
/// Tests run against `MockNativeContext`, whose `read_var_handle_root`
/// returns `None` — readers fall back to the cached raw ref — so a dummy
/// identity key is fine here (no moving GC in unit tests).
///
/// `ctx` is required for the same reason production callers need one: the slot
/// belongs to a VM, and there is no "current VM" to infer.
#[cfg(test)]
pub(crate) fn set_security_manager_for_test(
    ctx: &dyn NativeContext,
    sm: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    with_security_state(|table| {
        let state = table.entry(vm).or_default();
        let prev = state.security_manager.map(|(_, obj)| obj);
        state.security_manager = sm.map(|obj| (0, obj));
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
// GC: stored as `(identity_key, ObjectRef)` in the owning VM's row — kept alive
// + registry-remapped via `register_var_handle_root`, AND scanned/repointed
// directly by `gc_scan_security_manager_roots` /
// `gc_update_security_manager_refs`. Reads still re-fetch the CURRENT address
// via `read_var_handle_root(identity_key)`.

/// Read the Java `Policy` object installed in the CALLING VM, if any. Re-reads
/// the CURRENT (post-GC) address from the var-handle-root registry; contexts
/// without a registry (mocks) fall back to the cached raw ref.
fn get_policy_object(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let slot = security_slot(ctx, Slot::PolicyObject)?;
    Some(resolve_slot(ctx, slot))
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
    // Keep alive + registry-remapped across GC moves (VarHandle-root pattern).
    let entry = p.map(|obj| root_for_cache(ctx, obj));
    set_security_slot(&*ctx, Slot::PolicyObject, entry);
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
    // Keep alive + registry-remapped across GC moves (VarHandle-root pattern);
    // key computed on the just-registered address, no allocation in between.
    let entry = root_for_cache(ctx, p);
    let winner = publish_security_slot(&*ctx, Slot::PolicyObject, entry);
    Ok(resolve_slot(&*ctx, winner))
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
    if let Some(slot) = security_slot(&*ctx, Slot::SharedPermissions) {
        return Ok(resolve_slot(&*ctx, slot));
    }
    let perms = build_permissive_collection(ctx)?;
    // Keep alive + registry-remapped across GC moves (VarHandle-root pattern);
    // key computed on the just-registered address, no allocation in between.
    // The AllPermission entry in slot 0 stays live via normal heap tracing
    // from this root.
    let entry = root_for_cache(ctx, perms);
    let winner = publish_security_slot(&*ctx, Slot::SharedPermissions, entry);
    Ok(resolve_slot(&*ctx, winner))
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
    let perms = try_alloc_concurrent_synthetic(ctx, "java/security/Permissions", 2)?;
    let all_perm = try_alloc_concurrent_synthetic(ctx, "java/security/AllPermission", 0)?;
    // Slot 0 = the AllPermission entry (we reuse the synthetic Permissions
    // 2-field layout: [allPermission, readOnly]).
    ctx.set_field(perms, 0, Value::Object(Some(all_perm)));
    // Slot 1 = readOnly = true. `PermissionCollection.isReadOnly()` reads
    // this slot via `get_field_by_name("readOnly")`; the synthetic
    // Permissions field layout in `class_manager.rs` uses an int-backed
    // boolean (1 = read-only, 0 = mutable).
    ctx.set_field(perms, 1, Value::Int(1));
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
/// If object construction fails (e.g. the class can't be resolved in a
/// stripped runtime), fall back to the `RuntimeError::SecurityException`
/// path so a denial is still surfaced as *some* SecurityException rather
/// than silently allowed.
fn throw_access_control_exception(
    ctx: &mut dyn NativeContext,
    message: String,
) -> MethodCallFailed {
    let cls = "java/security/AccessControlException";
    // Allocate the exception object, then run its (String) constructor so
    // the detail message is populated the same way the JDK would.
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object(cls) {
        let msg_obj = ctx.create_string(&message);
        let ctor = ctx.invoke(
            cls,
            "<init>",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(exc)), Value::Object(Some(msg_obj))],
        );
        if ctor.is_ok() {
            return MethodCallFailed::ExceptionThrown(exc);
        }
    }
    // Fallback: still a SecurityException, just the base class.
    RuntimeError::SecurityException { message }.into()
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
        let message = format!("access denied (\"{class_name}\" \"{target}\" \"{actions}\")");
        Err(throw_access_control_exception(ctx, message))
    }
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

pub(crate) fn register_security_manager_natives(r: &mut NativeMethodRegistry) {
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
                    "access denied (\"java/lang/RuntimePermission\" \"createSecurityManager\")"
                        .to_string(),
                ));
            }
        }
        // Otherwise nothing to initialise: the object is a marker and the
        // global singleton is installed by `System.setSecurityManager`.
        Ok(None)
    });

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
        },
    );

    // checkPermission(Permission)V
    r.register(
        sm,
        "checkPermission",
        "(Ljava/security/Permission;)V",
        |ctx, args| {
            let perm = obj_arg(args, 1)?;
            check_permission_impl(ctx, perm)
        },
    );

    // checkRead(String)V — delegates to checkPermission with FilePermission("read")
    r.register(sm, "checkRead", "(Ljava/lang/String;)V", |ctx, args| {
        let _file = obj_arg(args, 1)?;
        // Create a synthetic FilePermission for the read action
        let perm = try_alloc_concurrent_synthetic(ctx, "java/io/FilePermission", 2)?;
        // field 0 = path (the file string), field 1 = actions
        ctx.set_field(perm, 0, args[1]);
        let actions = ctx.create_string("read");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));
        check_permission_impl(ctx, perm)
    });

    // checkWrite(String)V — delegates to checkPermission with FilePermission("write")
    r.register(sm, "checkWrite", "(Ljava/lang/String;)V", |ctx, args| {
        let _file = obj_arg(args, 1)?;
        let perm = try_alloc_concurrent_synthetic(ctx, "java/io/FilePermission", 2)?;
        ctx.set_field(perm, 0, args[1]);
        let actions = ctx.create_string("write");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));
        check_permission_impl(ctx, perm)
    });

    // checkConnect(String, int)V — delegates to checkPermission with SocketPermission
    r.register(sm, "checkConnect", "(Ljava/lang/String;I)V", |ctx, args| {
        let _host = obj_arg(args, 1)?;
        let perm = try_alloc_concurrent_synthetic(ctx, "java/net/SocketPermission", 2)?;
        ctx.set_field(perm, 0, args[1]); // host string
        let actions = ctx.create_string("connect");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));
        check_permission_impl(ctx, perm)
    });

    // checkExec(String)V — delegates to checkPermission with FilePermission("execute")
    r.register(sm, "checkExec", "(Ljava/lang/String;)V", |ctx, args| {
        let _cmd = obj_arg(args, 1)?;
        let perm = try_alloc_concurrent_synthetic(ctx, "java/io/FilePermission", 2)?;
        ctx.set_field(perm, 0, args[1]);
        let actions = ctx.create_string("execute");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));
        check_permission_impl(ctx, perm)
    });

    // checkDelete(String)V — delegates to checkPermission with FilePermission("delete")
    r.register(sm, "checkDelete", "(Ljava/lang/String;)V", |ctx, args| {
        let _file = obj_arg(args, 1)?;
        let perm = try_alloc_concurrent_synthetic(ctx, "java/io/FilePermission", 2)?;
        ctx.set_field(perm, 0, args[1]);
        let actions = ctx.create_string("delete");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));
        check_permission_impl(ctx, perm)
    });

    // checkPropertyAccess(String)V — delegates to checkPermission with PropertyPermission
    r.register(
        sm,
        "checkPropertyAccess",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let _prop = obj_arg(args, 1)?;
            let perm = try_alloc_concurrent_synthetic(ctx, "java/util/PropertyPermission", 2)?;
            ctx.set_field(perm, 0, args[1]); // property name
            let actions = ctx.create_string("read");
            ctx.set_field(perm, 1, Value::Object(Some(actions)));
            check_permission_impl(ctx, perm)
        },
    );

    // checkAccess(Thread)V — delegates to checkPermission with
    // RuntimePermission("modifyThread") so a loaded policy can deny it.
    // SECURITY FIX (V10): previously hardcoded `Ok(None)` (always allow),
    // which meant a restrictive java.policy could never constrain thread
    // access. Route through check_permission_impl like the other checkXxx
    // handlers; under the default (no policy) the impl still allows.
    r.register(sm, "checkAccess", "(Ljava/lang/Thread;)V", |ctx, args| {
        let _thread = obj_arg(args, 1)?;
        let perm = try_alloc_concurrent_synthetic(ctx, "java/lang/RuntimePermission", 2)?;
        let name = ctx.create_string("modifyThread");
        ctx.set_field(perm, 0, Value::Object(Some(name)));
        ctx.set_field(perm, 1, Value::Object(None)); // no actions
        check_permission_impl(ctx, perm)
    });

    // checkAccess(ThreadGroup)V — delegates to checkPermission with
    // RuntimePermission("modifyThreadGroup") so a loaded policy can deny it.
    // SECURITY FIX (V10): previously hardcoded `Ok(None)` (always allow).
    r.register(
        sm,
        "checkAccess",
        "(Ljava/lang/ThreadGroup;)V",
        |ctx, args| {
            let _group = obj_arg(args, 1)?;
            let perm = try_alloc_concurrent_synthetic(ctx, "java/lang/RuntimePermission", 2)?;
            let name = ctx.create_string("modifyThreadGroup");
            ctx.set_field(perm, 0, Value::Object(Some(name)));
            ctx.set_field(perm, 1, Value::Object(None)); // no actions
            check_permission_impl(ctx, perm)
        },
    );

    // checkCreateClassLoader()V — delegates to checkPermission with RuntimePermission
    r.register(sm, "checkCreateClassLoader", "()V", |ctx, args| {
        let perm = try_alloc_concurrent_synthetic(ctx, "java/lang/RuntimePermission", 2)?;
        let name = ctx.create_string("createClassLoader");
        ctx.set_field(perm, 0, Value::Object(Some(name)));
        ctx.set_field(perm, 1, Value::Object(None)); // no actions
        let _this = obj_arg(args, 0)?;
        check_permission_impl(ctx, perm)
    });

    // checkExit(int)V — delegates to checkPermission with RuntimePermission("exitVM.<code>")
    r.register(sm, "checkExit", "(I)V", |ctx, args| {
        let code = match args.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let perm = try_alloc_concurrent_synthetic(ctx, "java/lang/RuntimePermission", 2)?;
        let name = ctx.create_string(&format!("exitVM.{}", code));
        ctx.set_field(perm, 0, Value::Object(Some(name)));
        ctx.set_field(perm, 1, Value::Object(None)); // no actions
        check_permission_impl(ctx, perm)
    });

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
        },
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
        },
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
                        "access denied (\"java/lang/RuntimePermission\" \"setSecurityManager\")"
                            .to_string(),
                    ));
                }
            }
            let sm = match args.get(0) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            set_security_manager(ctx, sm);
            Ok(None)
        },
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
    parking_lot::RwLock<rustc_hash::FxHashMap<u32, Option<Arc<str>>>>,
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
    parking_lot::RwLock<rustc_hash::FxHashMap<u32, Arc<[Arc<str>]>>>,
> = OnceLock::new();

/// Cached form of [`action_code_base`] keyed by ClassId.  Returns the
/// same `Arc<str>` on every hit so the doPrivileged push is a refcount
/// bump.  See [`CLASS_CODE_BASE_CACHE`] for the rationale.
fn cached_action_code_base(ctx: &mut dyn NativeContext, class_id: ClassId) -> Option<Arc<str>> {
    let cache = CLASS_CODE_BASE_CACHE
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
    if let Some(slot) = cache.read().get(&class_id.as_u32()) {
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
    cache.write().insert(class_id.as_u32(), computed.clone());
    computed
}

/// Cached form of [`action_cert_digests`] keyed by ClassId.  The
/// PKCS#7 DER walk in [`x509::parse_signer_dn`] runs at most once per
/// class for the lifetime of the JVM; subsequent doPrivileged calls
/// reuse the same `Arc<[Arc<str>]>`.
fn cached_signer_tokens(ctx: &mut dyn NativeContext, class_id: ClassId) -> Arc<[Arc<str>]> {
    let cache = SIGNER_TOKENS_CACHE
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
    if let Some(arc) = cache.read().get(&class_id.as_u32()).cloned() {
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
    cache.write().insert(class_id.as_u32(), Arc::clone(&arc));
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
        |ctx, args| {
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
        },
    );

    // doPrivileged(PrivilegedExceptionAction)Ljava/lang/Object;
    // Invokes action.run() which may throw a checked exception.
    r.register(
        ac,
        "doPrivileged",
        "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;",
        |ctx, args| {
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
            // MEASURED by `apps/probes/SecuritySurfaceSweep.java`.
            wrap_checked_in_privileged_action_exception(ctx, result)
        },
    );

    // doPrivileged(PrivilegedAction, AccessControlContext)Ljava/lang/Object;
    // The AccessControlContext caps the privileges; for our model we respect
    // it by pushing the *action's* code base as the privileged frame.
    r.register(
        ac,
        "doPrivileged",
        "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
        |ctx, args| {
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
        },
    );

    // getContext()Ljava/security/AccessControlContext;
    // Returns a synthetic AccessControlContext with no protection-domain restrictions.
    r.register(
        ac,
        "getContext",
        "()Ljava/security/AccessControlContext;",
        |ctx, _args| {
            let acc = try_alloc_concurrent_synthetic(ctx, "java/security/AccessControlContext", 1)?;
            ctx.set_field(acc, 0, Value::Object(None)); // no protection domains
            Ok(Some(Value::Object(Some(acc))))
        },
    );

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
        native_ac_get_stack_access_control_context,
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
        native_ac_get_inherited_access_control_context,
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
        native_ac_get_protection_domain,
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
        native_ac_ensure_materialized_for_stack_walk,
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

fn register_access_control_context(r: &mut NativeMethodRegistry) {
    let acc = "java/security/AccessControlContext";

    // checkPermission(Permission)V — allow all under default policy
    r.register(
        acc,
        "checkPermission",
        "(Ljava/security/Permission;)V",
        |ctx, args| {
            let perm = obj_arg(args, 1)?;
            check_permission_impl(ctx, perm)
        },
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
    });

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
        },
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
    });

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
    });

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
        },
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
        },
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
        },
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
    });

    // <init>()V — KEEP (genuinely empty): Policy is abstract in real JDK so
    // `new Policy()` would never compile, but JBoss Modules subclasses it and
    // the subclass ctor invokes `super.<init>()`. Real
    // `java.security.Policy()`'s body IS empty (the class holds only static
    // state), so an empty native is exact, not a stub — it exists purely so
    // the invokespecial path doesn't fall through to the missing-method
    // branch.
    r.register(p, "<init>", "()V", |_ctx, _args| Ok(None));
    ()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    #[test]
    fn test_set_and_get_security_manager() {
        let _guard = security_state_test_lock();
        let mut ctx = MockNativeContext::new();
        // Reset global state
        set_security_manager(&mut ctx, None);

        // Initially null
        assert!(get_security_manager(&ctx).is_none());

        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

        // Set the SM
        set_security_manager(&mut ctx, Some(sm_obj));
        assert_eq!(get_security_manager(&ctx), Some(sm_obj));

        // Clear it
        set_security_manager(&mut ctx, None);
        assert!(get_security_manager(&ctx).is_none());
    }

    #[test]
    fn test_security_manager_init() {
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "<init>",
            "()V",
            &[Value::Object(Some(sm_obj))],
        );
        assert!(result.is_ok());
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "doPrivileged must release its action root"
        );
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_check_permission_allows_all() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let perm =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/AllPermission", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
        );
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_read_allows_all() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let file_str = ctx.create_string("/etc/passwd");

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkRead",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(file_str))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_connect_allows_all() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let host = ctx.create_string("example.com");

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkConnect",
            "(Ljava/lang/String;I)V",
            &[
                Value::Object(Some(sm_obj)),
                Value::Object(Some(host)),
                Value::Int(80),
            ],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_exit_allows_all() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkExit",
            "(I)V",
            &[Value::Object(Some(sm_obj)), Value::Int(0)],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_get_security_context() {
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "getSecurityContext",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(sm_obj))],
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(val.is_some(), "getSecurityContext should return an object");
        match val.unwrap() {
            Value::Object(Some(_)) => {} // expected
            other => panic!("Expected Object(Some(_)), got {:?}", other),
        }
    }

    #[test]
    fn test_system_get_set_security_manager() {
        let _guard = security_state_test_lock();

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        // Reset this VM's state (the mock reports identity 0).
        let _ = set_security_manager_for_test(&ctx, None);

        // getSecurityManager() should return null initially
        let get_sm = registry
            .find(
                "java/lang/System",
                "getSecurityManager",
                "()Ljava/lang/SecurityManager;",
            )
            .expect("getSecurityManager should be registered");

        let result = get_sm(&mut ctx, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(Value::Object(None)));

        // Set a SecurityManager
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let set_sm = registry
            .find(
                "java/lang/System",
                "setSecurityManager",
                "(Ljava/lang/SecurityManager;)V",
            )
            .expect("setSecurityManager should be registered");

        let result = set_sm(&mut ctx, &[Value::Object(Some(sm_obj))]);
        assert!(result.is_ok());

        // Now getSecurityManager should return it
        let result = get_sm(&mut ctx, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(Value::Object(Some(sm_obj))));

        // Clean up
        let _ = set_security_manager_for_test(&ctx, None);
    }

    #[test]
    fn test_do_privileged_invokes_run() {
        // FIX (parallel-safety): `doPrivileged` writes the action class's
        // codeBase/signer tokens into the *process-global* per-ClassId
        // caches (`CLASS_CODE_BASE_CACHE` / `SIGNER_TOKENS_CACHE`), keyed by
        // a `ClassId` that every fresh `MockNativeContext` restarts at a low
        // value. Without serialization this test can insert an entry for the
        // same `ClassId` another doPrivileged test is asserting on,
        // corrupting `test_do_privileged_pushes_and_pops_stack`. Acquire the
        // shared lock and clear the caches so all stack/cache mutators run
        // serially — without changing any production semantics.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let action =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivilegedAction", 0).unwrap();

        // Pre-arm the mock to return a value from invoke_virtual (action.run())
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
        }

        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "doPrivileged",
            "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
            &[Value::Object(Some(action))],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_do_privileged_exception_action() {
        // FIX (parallel-safety): see `test_do_privileged_invokes_run`.
        // doPrivileged mutates the global per-ClassId caches; serialize via
        // the shared test lock so it cannot race
        // `test_do_privileged_pushes_and_pops_stack`.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let action =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivilegedExceptionAction", 0)
                .unwrap();

        // Pre-arm: return Int(42) from action.run()
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(42))));
        }

        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "doPrivileged",
            "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;",
            &[Value::Object(Some(action))],
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(Value::Int(42)));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "exception-action doPrivileged must release its action root"
        );
    }

    #[test]
    fn test_do_privileged_with_context() {
        // FIX (parallel-safety): see `test_do_privileged_invokes_run`.
        // doPrivileged mutates the global per-ClassId caches; serialize via
        // the shared test lock so it cannot race
        // `test_do_privileged_pushes_and_pops_stack`.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let action =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/PrivilegedAction", 0).unwrap();
        let acc = try_alloc_concurrent_synthetic(&mut ctx, "java/security/AccessControlContext", 1)
            .unwrap();

        // Pre-arm
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
        }

        let result = call_native(
            &registry, &mut ctx,
            "java/security/AccessController", "doPrivileged",
            "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
            &[Value::Object(Some(action)), Value::Object(Some(acc))],
        );
        assert!(result.is_ok());
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "context overload must release its action root"
        );
    }

    #[test]
    fn test_access_controller_get_context() {
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "getContext",
            "()Ljava/security/AccessControlContext;",
            &[],
        );
        assert!(result.is_ok());
        let val = result.unwrap();
        assert!(val.is_some());
        match val.unwrap() {
            Value::Object(Some(_)) => {}
            other => panic!("Expected Object(Some(_)), got {:?}", other),
        }
    }

    #[test]
    fn test_access_control_context_check_permission() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let acc = try_alloc_concurrent_synthetic(&mut ctx, "java/security/AccessControlContext", 1)
            .unwrap();
        let perm =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/AllPermission", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessControlContext",
            "checkPermission",
            "(Ljava/security/Permission;)V",
            &[Value::Object(Some(acc)), Value::Object(Some(perm))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_create_class_loader() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkCreateClassLoader",
            "()V",
            &[Value::Object(Some(sm_obj))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_write_and_delete() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let file_str = ctx.create_string("/tmp/test.txt");

        // checkWrite
        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkWrite",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(file_str))],
        );
        assert!(result.is_ok());

        // checkDelete
        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkDelete",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(file_str))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_exec() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let cmd = ctx.create_string("/usr/bin/ls");

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkExec",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(cmd))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_property_access() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let prop = ctx.create_string("java.home");

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkPropertyAccess",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(prop))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_check_access_thread_and_group() {
        // `checkAccess` has routed through `check_permission_impl` since the V10
        // security fix, so this test reads the process-global `ACTIVE_POLICY`
        // like every other `checkXxx` test — it asserts the no-policy ALLOW.
        // Without the lock it was the one policy-sensitive test in this module
        // running unserialized, and a policy-installing sibling scheduled
        // alongside it turned the ALLOW into a DENY: `assert!(result.is_ok())`
        // failed roughly once in twenty whole-suite runs, and 27 times in 30
        // when the two are run as a pair on two threads.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let thread = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Thread", 0).unwrap();
        let group = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ThreadGroup", 0).unwrap();

        // checkAccess(Thread)
        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkAccess",
            "(Ljava/lang/Thread;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(thread))],
        );
        assert!(result.is_ok());

        // checkAccess(ThreadGroup)
        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkAccess",
            "(Ljava/lang/ThreadGroup;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(group))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_registration_count() {
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);
        // 13 SM methods + 2 System methods + 4 AC methods + 1 ACC method = 20
        // T19 · N3 adds 4 more AC methods (getStackAccessControlContext,
        // getInheritedAccessControlContext, getProtectionDomain(Class),
        // ensureMaterializedForStackWalk) → 24.
        // T19.H9 adds 9 more Policy methods (getPolicy, getPolicyNoCheck,
        // isSet, setPolicy, getPermissions(PD), getPermissions(CS),
        // implies, refresh, <init>) → 33.
        assert!(
            registry.len() >= 33,
            "Expected at least 33 registered natives, got {}",
            registry.len()
        );
    }

    // -----------------------------------------------------------------------
    // Policy integration + stack-walk semantics
    // -----------------------------------------------------------------------

    /// Serialize tests that mutate the global policy so parallel test
    /// execution doesn't cause one test's `clear_policy_and_stack()` to
    /// race another's `set_active_policy(Some(...))`.
    ///
    /// **Readers need it too, not just writers.** `ACTIVE_POLICY` is a process
    /// global, so a test that merely *observes* the no-policy default is racing
    /// every policy-installing sibling — the writers taking this lock among
    /// themselves does nothing for a reader that does not. Since the V10
    /// security fix every `checkXxx` native routes through
    /// `check_permission_impl`, which reads `ACTIVE_POLICY`; so the rule is:
    /// **a test that calls any `checkXxx` native, or `set_active_policy`, or
    /// `clear_policy_and_stack`, holds this guard for its whole body.**
    /// `test_check_access_thread_and_group` was the one test in this module
    /// that did not, and it failed about once in twenty whole-suite runs.
    fn policy_test_lock() -> std::sync::MutexGuard<'static, ()> {
        security_state_test_lock()
    }

    /// Reset global state between policy-sensitive tests.
    ///
    /// The object slots are per-VM now; every `MockNativeContext` reports
    /// `vm_identity() == 0` unless a test overrides it, so clearing scope 0 is
    /// exactly what the old whole-static clear did for these tests.
    fn clear_policy_and_stack() {
        set_active_policy(None);
        // Clear the mock VM's Policy slot directly (no ctx needed for a clear —
        // registration only happens on store of Some). The shared permission
        // collection goes too, so a stale ObjectRef from a prior test's
        // MockNativeContext heap doesn't leak into the next test — calls to
        // `ensure_shared_permission_collection` must allocate fresh.
        with_security_state(|table| {
            if let Some(state) = table.get_mut(&0) {
                state.policy_object = None;
                state.shared_permissions = None;
                if state.is_empty() {
                    table.remove(&0);
                }
            }
        });
        PRIVILEGED_STACK.with(|s| s.borrow_mut().clear());
        // Tests reuse small `ClassId` values across `MockNativeContext`
        // instances (each fresh ctx starts at id=1), so the per-ClassId
        // caches from prior tests would leak across the test boundary.
        // Clear them under the same gate as the privileged stack.
        if let Some(cache) = CLASS_CODE_BASE_CACHE.get() {
            cache.write().clear();
        }
        if let Some(cache) = SIGNER_TOKENS_CACHE.get() {
            cache.write().clear();
        }
    }

    #[test]
    fn test_policy_denies_when_class_not_granted() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        // Policy grants FilePermission only.
        let src = r#"
            grant {
                permission java.io.FilePermission "/tmp/*", "read";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        // Ask for a Socket permission — not granted.
        let perm =
            try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
        let host = ctx.create_string("example.com:443");
        ctx.set_field(perm, 0, Value::Object(Some(host)));
        let actions = ctx.create_string("connect");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
        );
        assert!(result.is_err(), "expected denial; got {result:?}");

        clear_policy_and_stack();
    }

    #[test]
    fn test_policy_allows_exact_grant() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"
            grant {
                permission java.io.FilePermission "/tmp/*", "read,write";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let perm = try_alloc_concurrent_synthetic(&mut ctx, "java/io/FilePermission", 2).unwrap();
        let target = ctx.create_string("/tmp/foo.txt");
        ctx.set_field(perm, 0, Value::Object(Some(target)));
        let actions = ctx.create_string("read");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
        );
        assert!(result.is_ok(), "expected allow; got {result:?}");

        clear_policy_and_stack();
    }

    #[test]
    fn test_policy_all_permission_fallback() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"grant { permission java.security.AllPermission; };"#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let perm =
            try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
        let host = ctx.create_string("example.com:80");
        ctx.set_field(perm, 0, Value::Object(Some(host)));
        let actions = ctx.create_string("connect");
        ctx.set_field(perm, 1, Value::Object(Some(actions)));

        let result = call_native(
            &registry,
            &mut ctx,
            "java/lang/SecurityManager",
            "checkPermission",
            "(Ljava/security/Permission;)V",
            &[Value::Object(Some(sm_obj)), Value::Object(Some(perm))],
        );
        assert!(result.is_ok());
        clear_policy_and_stack();
    }

    #[test]
    fn test_do_privileged_pushes_and_pops_stack() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        assert_eq!(privileged_stack_depth(), 0);

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        // Class name: com/acme/PrivilegedBlob
        let action =
            try_alloc_concurrent_synthetic(&mut ctx, "com/acme/PrivilegedBlob", 0).unwrap();

        // While action.run() executes, the stack depth should be 1 and the
        // top frame should reflect com/acme/PrivilegedBlob.
        let observed: std::sync::Arc<std::sync::Mutex<(usize, Option<String>)>> =
            std::sync::Arc::new(std::sync::Mutex::new((0, None)));
        let obs_clone = observed.clone();
        // Install a callback on MockNativeContext.invoke_virtual_result via a
        // lambda that captures the observation. MockNativeContext doesn't
        // take a closure, so instead we observe inside the native itself by
        // hooking the pre-armed result.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Object(None))));
        }

        // Snapshot the stack immediately before invoking doPrivileged by
        // manually pushing a sentinel and popping it after, then verifying
        // current_privileged_code_base() observed the action's code base
        // inside the native. Easier: intercept by calling push directly and
        // observing current_privileged_code_base.
        push_privileged_frame(Some("class:outer/Caller".into()));
        assert_eq!(
            current_privileged_code_base().as_deref(),
            Some("class:outer/Caller")
        );
        pop_privileged_frame();
        assert_eq!(privileged_stack_depth(), 0);

        // Now invoke doPrivileged and verify that, after completion, the
        // stack has unwound back to empty (i.e. push and pop both happened).
        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "doPrivileged",
            "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
            &[Value::Object(Some(action))],
        );
        assert!(result.is_ok());
        assert_eq!(
            privileged_stack_depth(),
            0,
            "doPrivileged must pop its frame"
        );

        // Direct check: push the same frame the wrapper would have and
        // confirm the observable mapping.
        push_privileged_frame(action_code_base(&mut ctx, action));
        assert_eq!(
            current_privileged_code_base().as_deref(),
            Some("class:com/acme/PrivilegedBlob")
        );
        pop_privileged_frame();

        let _ = obs_clone; // silence unused
        clear_policy_and_stack();
    }

    /// A -> B.doPrivileged(C -> checkPermission) must see B's code base,
    /// not A's. We model the chain manually: push A, call doPrivileged on
    /// an action whose class stands in for B's inner class, and assert the
    /// permission check sees B's code base via `current_privileged_code_base`.
    #[test]
    fn test_do_privileged_stack_walk_semantics() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();

        // Policy: grant FilePermission to code under com/acme only.
        let src = r#"
            grant codeBase "class:com/acme/-" {
                permission java.io.FilePermission "/tmp/*", "read";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();

        // A — a class *outside* com/acme — pushes itself as the "caller" frame.
        push_privileged_frame(Some("class:com/bogus/Attacker".into()));

        // Without doPrivileged, a checkPermission would see com/bogus/Attacker
        // and be denied. Verify this is the observed code base.
        assert_eq!(
            current_privileged_code_base().as_deref(),
            Some("class:com/bogus/Attacker")
        );

        // Now B (under com/acme) calls doPrivileged with its own action class.
        let b_action = try_alloc_concurrent_synthetic(&mut ctx, "com/acme/Helper$1", 0).unwrap();

        // Inside the doPrivileged invocation, our wrapper must push B's code
        // base — overriding the caller's. We verify this by intercepting
        // `invoke_virtual`. MockNativeContext returns a fixed pre-armed
        // value, so we emulate the inside-of-run observation by calling
        // `action_code_base` and pushing it ourselves, then checking the
        // top frame is B's, not A's.
        push_privileged_frame(action_code_base(&mut ctx, b_action));
        assert_eq!(
            current_privileged_code_base().as_deref(),
            Some("class:com/acme/Helper$1"),
            "checkPermission inside doPrivileged must see B's code base, not A's"
        );

        // Policy check inside the doPrivileged: FilePermission granted because
        // the privileged frame is under com/acme/-.
        assert!(policy_allows(
            "java/io/FilePermission",
            "/tmp/x",
            "read",
            current_privileged_code_base().as_deref(),
        ));

        // Pop B's frame; now we're back to A — the same check must now be
        // denied because com/bogus is not covered by the grant.
        pop_privileged_frame();
        assert!(!policy_allows(
            "java/io/FilePermission",
            "/tmp/x",
            "read",
            current_privileged_code_base().as_deref(),
        ));

        // Clean up A's frame.
        pop_privileged_frame();
        assert_eq!(privileged_stack_depth(), 0);
        clear_policy_and_stack();
    }

    // -----------------------------------------------------------------------
    // T11 · Real ProtectionDomain / CodeSource wiring + CIDR / signedBy
    // -----------------------------------------------------------------------

    #[test]
    fn t11_protection_domain_populated_from_jar_url() {
        // When a class is loaded from a JAR on disk, its CodeSource URL
        // flows end-to-end into the policy engine and matches a
        // `grant codeBase "file:/..."` clause.
        //
        // We exercise the ClassManager + ClassPath pipeline directly to
        // confirm that define_class() attaches a CodeSource with the
        // right URL; the security_manager-side behavior is covered by
        // the SM trait method (see `t11_action_code_base_reads_real_url`).
        use cratonvm_classloading::{ClassLoaderId, ClassManager};
        use std::io::Write as _;

        // Build a minimal JAR with one fake .class entry.
        let dir = std::env::temp_dir().join("cratonvm-t11-pd-jar");
        let _ = std::fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        {
            let f = std::fs::File::create(&jar_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            // Minimal valid class file: we won't actually parse it, but
            // ClassPath.find_class_code_source_info doesn't care about
            // parsing — it just checks archive membership.
            zw.start_file("com/acme/Foo.class", opts).unwrap();
            zw.write_all(b"\xCA\xFE\xBA\xBE_fake").unwrap();
            zw.finish().unwrap();
        }

        let app_cp = vec![jar_path.to_string_lossy().into_owned()];
        let cm = ClassManager::new(&[], &[], &app_cp);
        // find_class_code_source walks the classpath entries; returns a
        // CodeSource whose URL is `file:/<absolute path to jar>`.
        let cs = cm
            .find_class_code_source("com/acme/Foo")
            .expect("JAR-hosted class should have a real CodeSource");
        let url = cs.url.expect("URL should be present");
        assert!(
            url.starts_with("file:/"),
            "URL should use file: scheme, got {url}"
        );
        assert!(
            url.to_lowercase().ends_with("app.jar"),
            "URL should end with the JAR name, got {url}"
        );
        // Unsigned JAR → no certificate digests.
        assert!(cs.certificates.is_empty());
        assert!(cs.certificate_sha256.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t11_protection_domain_from_directory() {
        // A class loaded from a plain directory gets a `file:/.../` URL.
        use cratonvm_classloading::ClassManager;

        let dir = std::env::temp_dir().join("cratonvm-t11-pd-dir");
        let sub = dir.join("com").join("acme");
        let _ = std::fs::create_dir_all(&sub);
        std::fs::write(sub.join("Bar.class"), b"\xCA\xFE\xBA\xBE").unwrap();

        let app_cp = vec![dir.to_string_lossy().into_owned()];
        let cm = ClassManager::new(&[], &[], &app_cp);

        let cs = cm
            .find_class_code_source("com/acme/Bar")
            .expect("directory-hosted class should have a CodeSource");
        let url = cs.url.expect("URL present");
        assert!(url.starts_with("file:/"));
        assert!(url.ends_with('/'), "directory URL should end with /");
        assert!(cs.certificates.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t11_socket_permission_cidr_matches() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"
            grant {
                permission java.net.SocketPermission "10.0.0.0/8:80", "connect";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));
        // Every address inside 10.0.0.0/8 on port 80 is allowed.
        assert!(policy_allows(
            "java.net.SocketPermission",
            "10.1.2.3:80",
            "connect",
            None
        ));
        assert!(policy_allows(
            "java.net.SocketPermission",
            "10.255.255.254:80",
            "connect",
            None
        ));
        // Outside the CIDR — denied.
        assert!(!policy_allows(
            "java.net.SocketPermission",
            "11.0.0.1:80",
            "connect",
            None
        ));
        // Wrong port — denied even inside the CIDR.
        assert!(!policy_allows(
            "java.net.SocketPermission",
            "10.1.2.3:443",
            "connect",
            None
        ));
        clear_policy_and_stack();
    }

    #[test]
    fn t11_socket_permission_port_range() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"
            grant {
                permission java.net.SocketPermission "service.example:8080-8090", "connect";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));
        for port in 8080..=8090 {
            assert!(
                policy_allows(
                    "java.net.SocketPermission",
                    &format!("service.example:{port}"),
                    "connect",
                    None
                ),
                "port {port} should be allowed"
            );
        }
        assert!(!policy_allows(
            "java.net.SocketPermission",
            "service.example:8079",
            "connect",
            None
        ));
        assert!(!policy_allows(
            "java.net.SocketPermission",
            "service.example:8091",
            "connect",
            None
        ));
        clear_policy_and_stack();
    }

    #[test]
    fn t11_signed_by_grant_applies_only_to_signer_cert() {
        // Two classes, one "signed" (has a known SHA-256 digest) and
        // one not. A `grant signedBy "DEADBEEF"` block must apply only
        // when the calling frame's cert digests include "deadbeef".
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"
            grant signedBy "deadbeef" {
                permission java.io.FilePermission "/secret/*", "read";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        // Unsigned caller — denied.
        assert!(!policy_allows_full(
            "java.io.FilePermission",
            "/secret/passwords.txt",
            "read",
            None,
            &[],
        ));
        // Wrong signer — denied.
        assert!(!policy_allows_full(
            "java.io.FilePermission",
            "/secret/passwords.txt",
            "read",
            None,
            &["abc123".to_string()],
        ));
        // Signed with the matching digest — allowed.
        assert!(policy_allows_full(
            "java.io.FilePermission",
            "/secret/passwords.txt",
            "read",
            None,
            &["deadbeef".to_string()],
        ));
        // Case-insensitive match: "DEADBEEF" in the frame still matches.
        assert!(policy_allows_full(
            "java.io.FilePermission",
            "/secret/passwords.txt",
            "read",
            None,
            &["DEADBEEF".to_string()],
        ));
        clear_policy_and_stack();
    }

    #[test]
    fn t11_signed_by_with_code_base_combined() {
        // Both filters must hold: correct signedBy AND matching codeBase.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"
            grant codeBase "file:/opt/app.jar" signedBy "acme" {
                permission java.lang.RuntimePermission "getClassLoader";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        // Correct codeBase + correct signer → allowed.
        assert!(policy_allows_full(
            "java.lang.RuntimePermission",
            "getClassLoader",
            "",
            Some("file:/opt/app.jar"),
            &["acme".to_string()],
        ));
        // Correct codeBase, wrong signer → denied.
        assert!(!policy_allows_full(
            "java.lang.RuntimePermission",
            "getClassLoader",
            "",
            Some("file:/opt/app.jar"),
            &["other".to_string()],
        ));
        // Correct signer, wrong codeBase → denied.
        assert!(!policy_allows_full(
            "java.lang.RuntimePermission",
            "getClassLoader",
            "",
            Some("file:/opt/malicious.jar"),
            &["acme".to_string()],
        ));
        clear_policy_and_stack();
    }

    #[test]
    fn t11_privileged_frame_carries_cert_digests() {
        // Push a frame with a URL + digests, then verify both
        // `current_privileged_code_base` and
        // `current_privileged_cert_digests` see them.
        let _guard = policy_test_lock();
        clear_policy_and_stack();

        push_privileged_frame_full(
            Some("file:/opt/app.jar".to_string()),
            vec!["abcd".to_string(), "ef01".to_string()],
        );
        assert_eq!(
            current_privileged_code_base().as_deref(),
            Some("file:/opt/app.jar")
        );
        let digests = current_privileged_cert_digests();
        assert_eq!(digests.len(), 2);
        assert_eq!(digests[0], "abcd");
        assert_eq!(digests[1], "ef01");

        pop_privileged_frame();
        assert!(current_privileged_code_base().is_none());
        assert!(current_privileged_cert_digests().is_empty());
        clear_policy_and_stack();
    }

    #[test]
    fn t11_jar_signer_blocks_are_hashed() {
        // Feed a JAR containing exactly one `META-INF/*.RSA` signer block
        // (with its companion `*.SF`) through the real
        // `ClassManager::find_class_code_source` →
        // `ClassPath::extract_jar_signer_blocks` pipeline and assert that
        // the discovery half locates exactly one signer block and threads
        // it into the verified-certificate collection.
        //
        // FIX (signer-block discovery): this test previously asserted the
        // *obsolete* pre-Task-#40 contract — that the raw `.RSA` bytes were
        // stored verbatim in `CodeSource.certificates` and hashed to a
        // SHA-256 hex digest. Task #40 replaced that with real PKCS#7 +
        // trust-chain verification (see
        // `classloading/src/class_path.rs::extract_jar_signer_blocks` and
        // `classloading/src/jar_signer.rs::verify_signer_block`): only
        // signer blocks that parse as PKCS#7 SignedData AND chain to a
        // trust anchor in the process-wide `default_trust_store()`
        // contribute parsed X.509 leaf certificates. A synthetic
        // placeholder block (not valid PKCS#7) is therefore *discovered*
        // but, like `jarsigner -verify` on an unsigned/garbage block,
        // contributes zero verified certificates.
        //
        // The production trust store is a sealed process-global `OnceLock`
        // that we cannot deterministically seed from a unit test (init
        // order races with the other JAR-loading tests in this binary), so
        // a real end-to-end "one *verified* cert" assertion is not
        // achievable here. We instead pin the genuine, deterministic
        // contract: one signer block is discovered, and an unverifiable
        // block yields no certificates (fail-closed, matching HotSpot).
        // The end-to-end verify-and-collect path with a real RSA-signed,
        // trust-anchored block is covered in
        // `jar_signer.rs::real_rsa_signed_block_with_trust_anchor_verifies`.
        use cratonvm_classloading::ClassManager;
        use std::io::Write as _;

        // Unique temp dir per run so parallel test invocations don't clash
        // on the shared JAR path.
        let dir = std::env::temp_dir().join(format!(
            "cratonvm-t11-signed-jar-{}-{}",
            std::process::id(),
            SIGNED_JAR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let jar_path = dir.join("signed.jar");

        // A placeholder signer block. It is intentionally *not* valid
        // PKCS#7 SignedData — exactly the shape of an untrusted/garbage
        // signature block that real verification must reject.
        let rsa_bytes = b"simulated-pkcs7-signature-block-bytes";
        {
            let f = std::fs::File::create(&jar_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("com/acme/Foo.class", opts).unwrap();
            zw.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            zw.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zw.write_all(b"Manifest-Version: 1.0\r\n").unwrap();
            zw.start_file("META-INF/SIGNER.SF", opts).unwrap();
            zw.write_all(b"Signature-Version: 1.0\r\n").unwrap();
            zw.start_file("META-INF/SIGNER.RSA", opts).unwrap();
            zw.write_all(rsa_bytes).unwrap();
            zw.finish().unwrap();
        }

        // FIX: independently confirm the *discovery* half — the portion the
        // original assertion was probing — actually finds exactly one
        // signer block in `META-INF`. The original test was failing because
        // it conflated discovery (which works) with verification (which
        // correctly drops the synthetic block). We assert discovery here
        // directly off the archive so a regression in extension matching
        // (case sensitivity, `.RSA`/`.DSA`/`.EC` suffixes, the `META-INF/`
        // prefix, or `.SF` pairing) is caught for the right reason.
        let discovered = count_meta_inf_signer_blocks(&jar_path);
        assert_eq!(
            discovered, 1,
            "exactly one META-INF/*.RSA signer block must be discovered"
        );

        let app_cp = vec![jar_path.to_string_lossy().into_owned()];
        let cm = ClassManager::new(&[], &[], &app_cp);
        let cs = cm
            .find_class_code_source("com/acme/Foo")
            .expect("signed JAR should yield a CodeSource");

        // The synthetic block is discovered but is not valid PKCS#7, so it
        // fails real verification → zero *verified* certificates. This is
        // the current, deterministic production contract (fail-closed),
        // and keeping `certificates` and `certificate_sha256` in lock-step
        // is the invariant `CodeSource::new` upholds.
        assert_eq!(
            cs.certificates.len(),
            0,
            "an unverifiable signer block must contribute no verified certs"
        );
        assert_eq!(
            cs.certificate_sha256.len(),
            cs.certificates.len(),
            "certificate_sha256 stays in lock-step with certificates"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX: standalone signer-block discovery probe used by
    /// `t11_jar_signer_blocks_are_hashed`. Mirrors the discovery half of
    /// `ClassPath::extract_jar_signer_blocks` (the `META-INF/` prefix +
    /// case-insensitive `.RSA`/`.DSA`/`.EC` suffix match, paired with a
    /// `.SF` companion) so the test verifies that the block is *found*
    /// independently of whether it passes PKCS#7 trust-chain verification.
    /// Returns the number of signer blocks that have a matching `.SF`.
    fn count_meta_inf_signer_blocks(jar_path: &std::path::Path) -> usize {
        use std::io::Read as _;
        let bytes = std::fs::read(jar_path).expect("read jar");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open jar");

        // Collect entry names once.
        let names: Vec<String> = (0..archive.len())
            .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
            .collect();

        // Map uppercase stem (e.g. "META-INF/SIGNER") → has-a-.SF, matching
        // the case-insensitive pairing the production walk performs.
        let mut sf_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
        for n in &names {
            let upper = n.to_ascii_uppercase();
            if upper.starts_with("META-INF/") {
                if let Some(stem) = upper.strip_suffix(".SF") {
                    sf_stems.insert(stem.to_string());
                }
            }
        }

        let mut count = 0usize;
        for n in &names {
            let upper = n.to_ascii_uppercase();
            if !upper.starts_with("META-INF/") {
                continue;
            }
            let is_block =
                upper.ends_with(".RSA") || upper.ends_with(".DSA") || upper.ends_with(".EC");
            if !is_block {
                continue;
            }
            // Must have a matching `.SF` companion (same stem) and be
            // non-empty — the same gate the production walk applies before
            // attempting verification.
            let stem = match upper.rsplit_once('.') {
                Some((s, _)) => s.to_string(),
                None => continue,
            };
            if !sf_stems.contains(&stem) {
                continue;
            }
            let mut data = Vec::new();
            // `by_name` yields `Result<_, ZipError>` while `read_to_end` yields
            // `Result<_, io::Error>`; match instead of `and_then` to avoid the
            // error-type mismatch.
            let present = match archive.by_name(n) {
                Ok(mut e) => e.read_to_end(&mut data).is_ok() && !data.is_empty(),
                Err(_) => false,
            };
            if present {
                count += 1;
            }
        }
        count
    }

    /// Sequence counter so each `t11_jar_signer_blocks_are_hashed` run (and
    /// any parallel invocation) gets a unique temp JAR path.
    static SIGNED_JAR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn test_load_policy_file_end_to_end() {
        use std::io::Write as IoWrite;
        let _guard = policy_test_lock();
        clear_policy_and_stack();

        let dir = std::env::temp_dir().join("cratonvm-sm-policy");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("load_test.policy");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                "grant codeBase \"file:/home/app/-\" {{\n    permission java.io.FilePermission \"/tmp/*\", \"read,write\";\n    permission java.net.SocketPermission \"*:80\", \"connect\";\n}};"
            )
            .unwrap();
        }

        load_policy_file(&path).unwrap();
        // Policy is installed → non-granted permission is denied when no
        // privileged frame is active.
        assert!(!policy_allows(
            "java/io/FilePermission",
            "/tmp/a",
            "read",
            None,
        ));
        // With a matching code base, it is allowed.
        assert!(policy_allows(
            "java/io/FilePermission",
            "/tmp/a",
            "read",
            Some("file:/home/app/lib/x.jar"),
        ));
        assert!(policy_allows(
            "java/net/SocketPermission",
            "api.example.com:80",
            "connect",
            Some("file:/home/app/core.jar"),
        ));

        let _ = std::fs::remove_file(&path);
        clear_policy_and_stack();
    }

    // -----------------------------------------------------------------------
    // T19 · N3 — AccessController stack / PD / materialize natives
    // -----------------------------------------------------------------------

    #[test]
    fn t19_n3_ac_get_stack_context_returns_null_when_no_security_manager() {
        let _guard = security_state_test_lock();

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        // No SecurityManager is installed; the spec-matching answer is null.
        let _ = set_security_manager_for_test(&ctx, None);
        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "getStackAccessControlContext",
            "()Ljava/security/AccessControlContext;",
            &[],
        );
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Some(Value::Object(None)),
            "Expected null AccessControlContext under no-SM policy"
        );
    }

    #[test]
    fn t19_n3_ac_get_inherited_context_returns_null() {
        // We don't track inherited ACs — null is canonical.
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "getInheritedAccessControlContext",
            "()Ljava/security/AccessControlContext;",
            &[],
        );
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Some(Value::Object(None)),
            "Expected null inherited AccessControlContext"
        );
    }

    #[test]
    fn t19_n3_ac_get_protection_domain_delegates_to_class_native() {
        // The AC native delegates to Class.getProtectionDomain0 via
        // ctx.invoke. MockNativeContext.invoke returns Ok(None) for every
        // call — our native normalises that to a null ProtectionDomain so
        // the caller sees the documented "no PD available" result. This
        // test also verifies the graceful-fallback path that is used when
        // Agent N1's Class.getProtectionDomain0 native is not yet
        // registered: the delegation must never surface an
        // UnsatisfiedLinkError to AccessController callers.
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let class_mirror = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Class", 0).unwrap();

        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "getProtectionDomain",
            "(Ljava/lang/Class;)Ljava/security/ProtectionDomain;",
            &[Value::Object(Some(class_mirror))],
        );
        assert!(
            result.is_ok(),
            "delegation must never surface UnsatisfiedLinkError; got {result:?}"
        );
        // Either (a) N1 has landed and returned a non-null PD, or (b) we
        // fell through to null — both are valid end-user observations.
        match result.unwrap() {
            Some(Value::Object(_)) => {} // ok: null or a real PD
            other => panic!("Expected Object value (null or PD), got {:?}", other),
        }
    }

    #[test]
    fn t19_n3_ac_get_protection_domain_null_class_returns_null() {
        // Null Class argument → null PD. We must not delegate at all in this
        // path (saves an allocation and an invoke round-trip).
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "getProtectionDomain",
            "(Ljava/lang/Class;)Ljava/security/ProtectionDomain;",
            &[Value::Object(None)],
        );
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Some(Value::Object(None)),
            "null Class arg should short-circuit to null PD"
        );
    }

    #[test]
    fn t19_n3_ac_ensure_materialized_for_stack_walk_is_noop() {
        // Genuine spec no-op: returns void (Ok(None)) and does not touch the
        // heap or global state. We verify both the return shape and that
        // neither a null nor a live object argument changes the result.
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();

        // Null-argument case.
        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "ensureMaterializedForStackWalk",
            "(Ljava/lang/Object;)V",
            &[Value::Object(None)],
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None, "void method must return None");

        // Live-object case.
        let obj = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 0).unwrap();
        let result = call_native(
            &registry,
            &mut ctx,
            "java/security/AccessController",
            "ensureMaterializedForStackWalk",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(obj))],
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None, "void method must return None");
    }

    // -----------------------------------------------------------------------
    // T19_H9_ANCHOR_POLICY_TESTS
    // java.security.Policy native-override tests
    //
    // These tests cover the lenient `setPolicy`/`getPolicy` contract
    // documented in `register_policy_natives`. Each test grabs the
    // global `policy_test_lock` so it runs serial w.r.t. other tests
    // that mutate the mock VM's Policy slot or the parsed `Policy` slot,
    // and clears state both at entry and exit so a panic in one test
    // doesn't leak singleton state into the next.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h9_set_policy_then_get_returns_stored_ref() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let policy_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();

        // setPolicy(p)
        let set_p = registry
            .find(
                "java/security/Policy",
                "setPolicy",
                "(Ljava/security/Policy;)V",
            )
            .expect("setPolicy should be registered");
        let result = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);

        // getPolicy() should return exactly the same reference
        let get_p = registry
            .find(
                "java/security/Policy",
                "getPolicy",
                "()Ljava/security/Policy;",
            )
            .expect("getPolicy should be registered");
        let result = get_p(&mut ctx, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(Value::Object(Some(policy_obj))));

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_set_policy_null_is_accepted() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        // First install a real Policy so we can verify null clears it.
        let policy_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
        let set_p = registry
            .find(
                "java/security/Policy",
                "setPolicy",
                "(Ljava/security/Policy;)V",
            )
            .unwrap();
        let _ = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);
        assert_eq!(get_policy_object(&ctx), Some(policy_obj));

        // Now setPolicy(null) — must not throw.
        let result = set_p(&mut ctx, &[Value::Object(None)]);
        assert!(result.is_ok(), "setPolicy(null) must not raise");

        // The slot is cleared; the next getPolicy() lazily allocates a default.
        assert_eq!(get_policy_object(&ctx), None);

        let get_p = registry
            .find(
                "java/security/Policy",
                "getPolicy",
                "()Ljava/security/Policy;",
            )
            .unwrap();
        let result = get_p(&mut ctx, &[]);
        assert!(result.is_ok());
        // getPolicy must return *some* object — the lazy default — never null.
        match result.unwrap() {
            Some(Value::Object(Some(_))) => {}
            other => panic!("expected lazy default Policy, got {:?}", other),
        }

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_get_policy_lazy_default_is_idempotent() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let get_p = registry
            .find(
                "java/security/Policy",
                "getPolicy",
                "()Ljava/security/Policy;",
            )
            .unwrap();

        // First call: lazy-allocate default.
        let r1 = get_p(&mut ctx, &[]).unwrap();
        // Second call: must return the same reference (idempotent).
        let r2 = get_p(&mut ctx, &[]).unwrap();
        assert_eq!(r1, r2, "getPolicy must be idempotent across calls");
        match r1 {
            Some(Value::Object(Some(_))) => {}
            other => panic!("expected non-null default, got {:?}", other),
        }

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_get_policy_no_check_matches_get_policy() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let policy_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
        let set_p = registry
            .find(
                "java/security/Policy",
                "setPolicy",
                "(Ljava/security/Policy;)V",
            )
            .unwrap();
        let _ = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);

        let get_p = registry
            .find(
                "java/security/Policy",
                "getPolicy",
                "()Ljava/security/Policy;",
            )
            .unwrap();
        let get_p_no_check = registry
            .find(
                "java/security/Policy",
                "getPolicyNoCheck",
                "()Ljava/security/Policy;",
            )
            .unwrap();
        let r1 = get_p(&mut ctx, &[]).unwrap();
        let r2 = get_p_no_check(&mut ctx, &[]).unwrap();
        assert_eq!(r1, r2, "getPolicyNoCheck must mirror getPolicy");

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_is_set_reflects_singleton_state() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let is_set = registry
            .find("java/security/Policy", "isSet", "()Z")
            .expect("isSet should be registered");

        // Initially: no Policy → false.
        let r = is_set(&mut ctx, &[]).unwrap();
        assert_eq!(r, Some(Value::Int(0)));

        // After install: true.
        let policy_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
        let set_p = registry
            .find(
                "java/security/Policy",
                "setPolicy",
                "(Ljava/security/Policy;)V",
            )
            .unwrap();
        let _ = set_p(&mut ctx, &[Value::Object(Some(policy_obj))]);
        let r = is_set(&mut ctx, &[]).unwrap();
        assert_eq!(r, Some(Value::Int(1)));

        // After clear: false.
        let _ = set_p(&mut ctx, &[Value::Object(None)]);
        let r = is_set(&mut ctx, &[]).unwrap();
        assert_eq!(r, Some(Value::Int(0)));

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_implies_returns_true() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let pd =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();
        let perm =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/AllPermission", 0).unwrap();

        let implies = registry
            .find(
                "java/security/Policy",
                "implies",
                "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
            )
            .expect("implies should be registered");
        let result = implies(
            &mut ctx,
            &[Value::Object(Some(pd)), Value::Object(Some(perm))],
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(Value::Int(1)));

        clear_policy_and_stack();
    }

    /// With a parsed policy installed, `Policy.implies(pd, perm)` must
    /// return 0 for a permission no grant covers and 1 for one that is
    /// covered — i.e. it genuinely delegates to the grant evaluation
    /// instead of unconditionally returning 1.
    #[test]
    fn policy_implies_delegates_to_parsed_policy() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let src = r#"
            grant {
                permission java.io.FilePermission "/tmp/*", "read";
            };
        "#;
        set_active_policy(Some(Policy::parse(src).unwrap()));

        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let implies = registry
            .find(
                "java/security/Policy",
                "implies",
                "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
            )
            .expect("implies should be registered");

        let mut ctx = MockNativeContext::new();
        let pd =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();

        // Not granted: a SocketPermission must be DENIED (implies -> 0).
        let denied =
            try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
        let host = ctx.create_string("example.com:443");
        ctx.set_field(denied, 0, Value::Object(Some(host)));
        let act = ctx.create_string("connect");
        ctx.set_field(denied, 1, Value::Object(Some(act)));
        let r = implies(
            &mut ctx,
            &[Value::Object(Some(pd)), Value::Object(Some(denied))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(0)), "ungranted permission must imply 0");

        // Granted: a FilePermission read on /tmp/* must be ALLOWED (-> 1).
        let granted =
            try_alloc_concurrent_synthetic(&mut ctx, "java/io/FilePermission", 2).unwrap();
        let path = ctx.create_string("/tmp/foo.txt");
        ctx.set_field(granted, 0, Value::Object(Some(path)));
        let act2 = ctx.create_string("read");
        ctx.set_field(granted, 1, Value::Object(Some(act2)));
        let r = implies(
            &mut ctx,
            &[Value::Object(Some(pd)), Value::Object(Some(granted))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(1)), "granted permission must imply 1");

        clear_policy_and_stack();
    }

    /// Sanity: when NO policy is installed (the BouncyCastle / default case)
    /// `Policy.implies` returns 1 (allow-all) for any permission.
    #[test]
    fn policy_implies_allows_all_with_no_policy() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let implies = registry
            .find(
                "java/security/Policy",
                "implies",
                "(Ljava/security/ProtectionDomain;Ljava/security/Permission;)Z",
            )
            .unwrap();

        let mut ctx = MockNativeContext::new();
        let pd =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();
        let perm =
            try_alloc_concurrent_synthetic(&mut ctx, "java/net/SocketPermission", 2).unwrap();
        let r = implies(
            &mut ctx,
            &[Value::Object(Some(pd)), Value::Object(Some(perm))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(1)));

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_refresh_is_noop() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let refresh = registry
            .find("java/security/Policy", "refresh", "()V")
            .expect("refresh should be registered");
        let result = refresh(&mut ctx, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None, "refresh must return void");

        // refresh must not touch the singleton state — install nothing
        // before the call and verify isSet stays false afterwards.
        assert_eq!(get_policy_object(&ctx), None);

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_get_permissions_protection_domain_returns_collection() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let pd =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();

        let get_perms = registry
            .find(
                "java/security/Policy",
                "getPermissions",
                "(Ljava/security/ProtectionDomain;)Ljava/security/PermissionCollection;",
            )
            .expect("getPermissions(ProtectionDomain) should be registered");
        let result = get_perms(&mut ctx, &[Value::Object(Some(pd))]);
        assert!(result.is_ok());
        let val = result.unwrap();
        match val {
            Some(Value::Object(Some(perms))) => {
                // Slot 1 holds the read-only flag; verify it was set on creation.
                let read_only = ctx.get_field(perms, 1);
                assert_eq!(
                    read_only,
                    Value::Int(1),
                    "permissive collection must be read-only"
                );
                // Slot 0 holds the AllPermission entry; non-null.
                match ctx.get_field(perms, 0) {
                    Value::Object(Some(_)) => {}
                    other => panic!("expected AllPermission entry, got {:?}", other),
                }
            }
            other => panic!("expected non-null PermissionCollection, got {:?}", other),
        }

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_get_permissions_code_source_returns_collection() {
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let cs = try_alloc_concurrent_synthetic(&mut ctx, "java/security/CodeSource", 2).unwrap();

        let get_perms = registry
            .find(
                "java/security/Policy",
                "getPermissions",
                "(Ljava/security/CodeSource;)Ljava/security/PermissionCollection;",
            )
            .expect("getPermissions(CodeSource) should be registered");
        let result = get_perms(&mut ctx, &[Value::Object(Some(cs))]);
        assert!(result.is_ok());
        match result.unwrap() {
            Some(Value::Object(Some(_))) => {} // collection returned
            other => panic!("expected non-null collection, got {:?}", other),
        }

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_get_permissions_returns_shared_collection() {
        // Two consecutive calls to getPermissions(...) must return the same
        // shared collection so accidental mutation is observable and we
        // don't allocate a fresh one on every invocation.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let pd =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/ProtectionDomain", 4).unwrap();

        let get_perms = registry
            .find(
                "java/security/Policy",
                "getPermissions",
                "(Ljava/security/ProtectionDomain;)Ljava/security/PermissionCollection;",
            )
            .unwrap();
        let r1 = get_perms(&mut ctx, &[Value::Object(Some(pd))]).unwrap();
        let r2 = get_perms(&mut ctx, &[Value::Object(Some(pd))]).unwrap();
        assert_eq!(
            r1, r2,
            "getPermissions must return the same shared collection"
        );

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_set_policy_does_not_throw_on_any_input() {
        // The T19.H9 contract: setPolicy must NEVER throw — JBoss Modules,
        // WildFly, EJBCA, and many other libraries call it during boot
        // and rely on it succeeding. Cover the three input shapes we
        // care about: real ref, null, and a wrong-type (Int) arg that
        // could come from a mis-shaped frame.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let policy_obj =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();

        let set_p = registry
            .find(
                "java/security/Policy",
                "setPolicy",
                "(Ljava/security/Policy;)V",
            )
            .unwrap();

        assert!(set_p(&mut ctx, &[Value::Object(Some(policy_obj))]).is_ok());
        assert!(set_p(&mut ctx, &[Value::Object(None)]).is_ok());
        // Wrong-type argument: native must be defensive and not panic.
        assert!(set_p(&mut ctx, &[Value::Int(42)]).is_ok());
        assert!(set_p(&mut ctx, &[]).is_ok(), "missing arg must not panic");

        clear_policy_and_stack();
    }

    #[test]
    fn t19_h9_set_security_manager_does_not_throw() {
        // Co-required path: JBoss Modules calls
        //   System.setSecurityManager(new SecurityManager())
        // immediately after Policy.setPolicy. This regression test guards
        // the existing System.setSecurityManager native against a future
        // refactor that re-introduces a throw — KC16 boot would then
        // regress from "advances past Policy stub" back to the
        // UnsupportedOperationException error.
        let _guard = policy_test_lock();
        clear_policy_and_stack();
        let mut registry = NativeMethodRegistry::new();
        register_security_manager_natives(&mut registry);

        let mut ctx = MockNativeContext::new();
        let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();

        let set_sm = registry
            .find(
                "java/lang/System",
                "setSecurityManager",
                "(Ljava/lang/SecurityManager;)V",
            )
            .unwrap();
        assert!(set_sm(&mut ctx, &[Value::Object(Some(sm))]).is_ok());
        assert!(set_sm(&mut ctx, &[Value::Object(None)]).is_ok());

        clear_policy_and_stack();
        let _ = set_security_manager_for_test(&ctx, None);
    }

    // -----------------------------------------------------------------------
    // Per-VM isolation of the SecurityManager / Policy state
    // -----------------------------------------------------------------------
    //
    // Every `MockNativeContext` reports `vm_identity() == 0` by default, which
    // is what the single-VM tests above exercise. These tests give each mock
    // its own identity — the same thing `SharedVm::vm_identity` does for two
    // real VMs in one process — so they are also parallel-safe and need no
    // shared test lock: no other test can address these rows.

    /// Distinct, unlikely-to-collide VM identities for the tests below.
    const VM_A: usize = 0x0C2_0001;
    const VM_B: usize = 0x0C2_0002;

    /// Build a mock context that reports `vm` as its VM identity.
    fn vm_scoped_ctx(vm: usize) -> MockNativeContext {
        let ctx = MockNativeContext::new();
        ctx.set_vm_identity(vm);
        ctx
    }

    #[test]
    fn two_vms_hold_independent_security_managers() {
        let mut a = vm_scoped_ctx(VM_A);
        let mut b = vm_scoped_ctx(VM_B);
        let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
        let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();

        // Installing in A must not be visible in B, and vice versa. Before the
        // per-VM keying this read handed B the raw `sm_a` address — a pointer
        // into a heap B's collector neither scans nor owns.
        set_security_manager(&mut a, Some(sm_a));
        assert_eq!(get_security_manager(&a), Some(sm_a));
        assert!(
            get_security_manager(&b).is_none(),
            "VM B must not see VM A's SecurityManager"
        );

        set_security_manager(&mut b, Some(sm_b));
        assert_eq!(get_security_manager(&a), Some(sm_a));
        assert_eq!(get_security_manager(&b), Some(sm_b));

        forget_vm_security_state(VM_A);
        forget_vm_security_state(VM_B);
    }

    #[test]
    fn clearing_one_vms_security_manager_leaves_the_other_armed() {
        let mut a = vm_scoped_ctx(VM_A + 0x10);
        let mut b = vm_scoped_ctx(VM_B + 0x10);
        let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
        let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();
        set_security_manager(&mut a, Some(sm_a));
        set_security_manager(&mut b, Some(sm_b));

        // This is the privilege-escalation-by-removal case: an unsandboxed VM
        // calling `System.setSecurityManager(null)` used to write `None` to the
        // shared slot and disarm the sandboxed VM's checkExec / loadLibrary
        // gates. It must now clear only the caller's VM.
        set_security_manager(&mut a, None);
        assert!(get_security_manager(&a).is_none());
        assert_eq!(
            get_security_manager(&b),
            Some(sm_b),
            "clearing VM A's manager must leave VM B armed"
        );

        forget_vm_security_state(VM_A + 0x10);
        forget_vm_security_state(VM_B + 0x10);
    }

    #[test]
    fn policy_and_permission_slots_are_per_vm() {
        let mut a = vm_scoped_ctx(VM_A + 0x20);
        let mut b = vm_scoped_ctx(VM_B + 0x20);

        let policy_a = try_alloc_concurrent_synthetic(&mut a, "java/security/Policy", 1).unwrap();
        set_policy_object(&mut a, Some(policy_a));
        assert_eq!(get_policy_object(&a), Some(policy_a));
        assert_eq!(get_policy_object(&b), None);
        assert!(policy_object_installed(&a));
        assert!(!policy_object_installed(&b));

        // The lazy default and the shared Permissions collection are per-VM
        // too: each VM gets its own, allocated from its own heap.
        let default_b = ensure_default_policy_object(&mut b).unwrap();
        assert_eq!(get_policy_object(&a), Some(policy_a));
        assert_eq!(get_policy_object(&b), Some(default_b));

        let perms_a = ensure_shared_permission_collection(&mut a).unwrap();
        let perms_b = ensure_shared_permission_collection(&mut b).unwrap();
        assert_eq!(
            ensure_shared_permission_collection(&mut a).unwrap(),
            perms_a,
            "identity of the shared collection must be stable within a VM"
        );
        assert_eq!(
            ensure_shared_permission_collection(&mut b).unwrap(),
            perms_b
        );

        forget_vm_security_state(VM_A + 0x20);
        forget_vm_security_state(VM_B + 0x20);
    }

    #[test]
    fn security_manager_ref_is_scanned_and_remapped_per_vm() {
        let vm_a = VM_A + 0x30;
        let vm_b = VM_B + 0x30;
        let mut a = vm_scoped_ctx(vm_a);
        let mut b = vm_scoped_ctx(vm_b);

        let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
        let policy_a = try_alloc_concurrent_synthetic(&mut a, "java/security/Policy", 1).unwrap();
        let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();
        set_security_manager(&mut a, Some(sm_a));
        set_policy_object(&mut a, Some(policy_a));
        set_security_manager(&mut b, Some(sm_b));

        // Scan: the collector must be told about the refs this module holds,
        // and ONLY about the ones belonging to the VM it is collecting.
        let mut roots = Vec::new();
        gc_scan_security_manager_roots(vm_a, &mut roots);
        assert!(roots.contains(&sm_a), "the installed SM must be a root");
        assert!(
            roots.contains(&policy_a),
            "the Policy object must be a root"
        );
        assert!(
            !roots.contains(&sm_b),
            "VM A's scan must not report VM B's objects"
        );

        // Remap: stand in for a moving collection that relocated `sm_a` onto a
        // second, genuinely-allocated object's address. An empty map is a
        // no-op (the non-moving sweep).
        let relocated =
            try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
        gc_update_security_manager_refs(vm_a, &cratonvm_types::PointerMap::default());
        assert_eq!(get_security_manager(&a), Some(sm_a));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(sm_a.as_ptr() as usize, relocated.as_ptr() as usize);
        gc_update_security_manager_refs(vm_a, &map);
        assert_eq!(
            get_security_manager(&a),
            Some(relocated),
            "the cached copy must follow the object, not stay on the vacated slot"
        );
        assert_eq!(
            get_policy_object(&a),
            Some(policy_a),
            "an object the collector did not move must be left alone"
        );
        assert_eq!(
            get_security_manager(&b),
            Some(sm_b),
            "VM A's remap must not touch VM B's slots"
        );

        forget_vm_security_state(vm_a);
        forget_vm_security_state(vm_b);
    }

    #[test]
    fn vm_teardown_does_not_disturb_the_other_vm() {
        let vm_a = VM_A + 0x40;
        let vm_b = VM_B + 0x40;
        let mut a = vm_scoped_ctx(vm_a);
        let mut b = vm_scoped_ctx(vm_b);
        let sm_a = try_alloc_concurrent_synthetic(&mut a, "java/lang/SecurityManager", 0).unwrap();
        let sm_b = try_alloc_concurrent_synthetic(&mut b, "java/lang/SecurityManager", 0).unwrap();
        let perms_b = ensure_shared_permission_collection(&mut b).unwrap();
        set_security_manager(&mut a, Some(sm_a));
        set_security_manager(&mut b, Some(sm_b));

        forget_vm_security_state(vm_a);

        assert!(
            get_security_manager(&a).is_none(),
            "teardown must drop the torn-down VM's slots"
        );
        let mut roots_a = Vec::new();
        gc_scan_security_manager_roots(vm_a, &mut roots_a);
        assert!(
            roots_a.is_empty(),
            "a torn-down VM must report no roots (its heap is gone)"
        );

        assert_eq!(get_security_manager(&b), Some(sm_b));
        assert_eq!(
            ensure_shared_permission_collection(&mut b).unwrap(),
            perms_b
        );
        let mut roots_b = Vec::new();
        gc_scan_security_manager_roots(vm_b, &mut roots_b);
        assert!(roots_b.contains(&sm_b));
        assert!(roots_b.contains(&perms_b));

        forget_vm_security_state(vm_b);
    }

    #[test]
    fn single_vm_behaviour_is_unchanged() {
        // One VM sees exactly the old semantics: install / read back / clear,
        // a stable lazy default Policy, a stable shared Permissions collection,
        // and `isSet` following the Policy slot.
        let vm = VM_A + 0x50;
        let mut ctx = vm_scoped_ctx(vm);

        assert!(get_security_manager(&ctx).is_none());
        let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        set_security_manager(&mut ctx, Some(sm));
        assert_eq!(get_security_manager(&ctx), Some(sm));
        set_security_manager(&mut ctx, None);
        assert!(get_security_manager(&ctx).is_none());

        assert!(!policy_object_installed(&ctx));
        let default_policy = ensure_default_policy_object(&mut ctx).unwrap();
        assert_eq!(
            ensure_default_policy_object(&mut ctx).unwrap(),
            default_policy,
            "the lazy default must be allocated exactly once per VM"
        );
        assert!(policy_object_installed(&ctx));
        assert_eq!(get_policy_object(&ctx), Some(default_policy));

        let installed =
            try_alloc_concurrent_synthetic(&mut ctx, "java/security/Policy", 1).unwrap();
        set_policy_object(&mut ctx, Some(installed));
        assert_eq!(get_policy_object(&ctx), Some(installed));
        set_policy_object(&mut ctx, None);
        assert_eq!(get_policy_object(&ctx), None);
        assert!(!policy_object_installed(&ctx));

        let perms = ensure_shared_permission_collection(&mut ctx).unwrap();
        assert_eq!(
            ensure_shared_permission_collection(&mut ctx).unwrap(),
            perms
        );

        forget_vm_security_state(vm);
    }
}

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
    let built = ctx.new_object_initialized(
        "java/security/PrivilegedActionException",
        "(Ljava/lang/Exception;)V",
        &[Value::Object(Some(thrown_now))],
    );
    let thrown_now = ctx.read_native_pin(pin, thrown_now);
    ctx.unpin_native_roots(pin);
    match built {
        Ok(Some(Value::Object(Some(wrapper)))) => {
            Err(MethodCallFailed::ExceptionThrown(wrapper))
        }
        _ => Err(MethodCallFailed::ExceptionThrown(thrown_now)),
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
