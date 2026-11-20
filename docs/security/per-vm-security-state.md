# Per-VM SecurityManager and Policy state

**Status:** landed in `native-builtins/src/security_manager.rs`. **GC
registration and teardown wiring are NOT yet in place** — see §5 and §6; until
they are, the scan/remap hooks are inert (no behaviour change) but the held
refs are only kept live by the pre-existing var-handle-root registration.

**Addresses:** the C2 review finding that `System.setSecurityManager` installs a
process-wide singleton (`docs/architecture/per-vm-state.md` §1,
`docs/security/native-capabilities.md` §"privilege escalation by removal").

---

## 1. What was wrong

Three statics in `native-builtins/src/security_manager.rs` held a Java object
for the whole process:

```rust
static SECURITY_MANAGER:            Mutex<Option<(i32, ObjectRef)>> = Mutex::new(None);
static ACTIVE_POLICY_OBJECT:        Mutex<Option<(i32, ObjectRef)>> = Mutex::new(None);
static SHARED_PERMISSION_COLLECTION: Mutex<Option<(i32, ObjectRef)>> = Mutex::new(None);
```

Each was written by `ctx.register_var_handle_root(obj)` plus a cached
`(identity_hash, ObjectRef)` pair, and read back as
`ctx.read_var_handle_root(key).unwrap_or(cached)`.

That read is correct in exactly one VM and wrong in every other one, because
the registry it consults is **per-VM**: `var_handle_roots` is a field of
`HeapRealm` (`vm/src/vm/realms/heap_realm.rs`), reached through
`SharedVm::mem`. The registry is per-VM; the cached slot was not.

Two consequences, both real:

1. **Use-after-move, not merely a policy leak.** In a second VM the
   `read_var_handle_root` lookup MISSES and `unwrap_or(cached)` returns the
   *installing* VM's raw `ObjectRef`. That address belongs to a heap the
   reading VM's collector never scans, and which the owning VM's collector
   cannot rewrite either — the owning collector rewrites its **registry entry**,
   not this static copy. Under a moving young GC the static then names a
   vacated from-space slot. `docs/threading/objectref-concurrency-contract.md`
   names this exact shape: an address holder that no root set covers and no
   pointer map rewrites.

2. **Cross-VM disarm.** `System.setSecurityManager(null)` wrote `None` to the
   shared slot. Since `check_exec_or_throw`
   (`native-builtins/src/lang_system.rs`) and
   `check_host_native_access_or_throw` (`security_manager.rs`) both return
   `Ok(())` when `get_security_manager(...)` is `None`, an unsandboxed VM A
   could disarm a sandboxed VM B's `Runtime.exec` / `ProcessBuilder.start` and
   `loadLibrary` gates unchallenged. The permission check that guards
   `setSecurityManager` consults only the *currently installed* manager, so
   there was nothing to refuse the call.

`ACTIVE_POLICY_OBJECT` and `SHARED_PERMISSION_COLLECTION` had the identical
shape, with the same `unwrap_or(cached)` reads, so a fix had to cover all
three.

## 2. What replaced them

One process-global **index**, keyed by VM identity:

```rust
struct VmSecurityState {
    security_manager:   Option<(i32, ObjectRef)>,
    policy_object:      Option<(i32, ObjectRef)>,
    shared_permissions: Option<(i32, ObjectRef)>,
}

static SECURITY_STATE: OnceLock<Mutex<HashMap<usize, VmSecurityState>>>;
```

The model is `native-api/src/capability.rs`'s `VM_CAPABILITIES`: a
process-global *index*, not a process-global *policy*. Nothing in it is
reachable without a `NativeContext` — every accessor
(`security_slot` / `set_security_slot`) derives the key from
`ctx.vm_identity()`, and no call site can supply a key of its own. A cross-VM
read is therefore not something the code refuses; it is something it cannot
express.

`vm_identity()` is the one VM-identity notion in the tree
(`SharedVm::vm_identity`, `vm/src/vm/vm_exec.rs:12202`), the same value
`vm/src/memory/native_roots.rs` already hands to the per-VM root sources.

## 3. Rooting

Modelled on `native-builtins/src/lang_math.rs`'s
`gc_scan_value_of_cache_roots` / `gc_update_value_of_cache_refs`:

```rust
pub fn gc_scan_security_manager_roots(vm_identity: usize, out: &mut Vec<ObjectRef>);
pub fn gc_update_security_manager_refs(vm_identity: usize, pointer_map: &HashMap<usize, usize>);
```

* The scan reports the three refs held for **that VM only** — a heap address
  means nothing outside the heap that produced it, and handing another VM's
  address to this collector would be a pointer into a heap it does not own.
* The remap repoints the cached copies through the collector's old→new map.
  This is the half that never existed: previously the registry entry was
  remapped and the static copy was not. Identity keys are untouched; an
  identity hash is stable across a move.

`register_var_handle_root` is still called on install. It is now belt *and*
braces rather than the only mechanism, and it keeps the read path
(`read_var_handle_root(key).unwrap_or(cached)`) unchanged.

### Lock discipline

The state mutex is **never held across a Java allocation**. The GC root scan
takes that same mutex at a safepoint, so a thread that allocated while holding
it could deadlock against its own collection. The two lazy initialisers
(`ensure_default_policy_object`, `ensure_shared_permission_collection`)
therefore allocate first and publish afterwards, under the lock, in one step.
No caller can observe a half-initialised object; if two threads race, the loser
drops its allocation and both return the same object, which is the identity
guarantee `Policy.getPermissions(pd1) == Policy.getPermissions(pd2)` depends
on. Previously the allocation happened *under* the singleton mutex — that was
safe only because no GC hook took it.

## 4. Teardown

```rust
pub fn forget_vm_security_state(vm_identity: usize);
```

Drops the VM's whole row. Without it, the raw heap addresses outlive the heap
that produced them, and a later VM that reused the identity would inherit a
dead SecurityManager. Nothing else in the process holds these refs — they are
not shared across VMs by construction — so removing the row is sufficient.

## 5. Registration required (NOT yet applied)

`vm/src/memory/native_roots.rs` owns the compile-time inventory of root
sources. It needs one paired entry, in the shape the existing per-VM sources
use:

```rust
fn scan_security_manager(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::security_manager::gc_scan_security_manager_roots(
        shared.vm_identity,
        roots,
    );
}
fn remap_security_manager(shared: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::security_manager::gc_update_security_manager_refs(
        shared.vm_identity,
        map,
    );
}
```

and, in `VM_ROOT_SOURCES`:

```rust
    root_source!(
        "security-manager",
        scan_security_manager,
        remap_security_manager
    ),
```

Until that lands, both hooks are dead code: the refs stay live via the
var-handle-root registry (as before this change), and the remap half — the
actual use-after-move fix for the single-VM moving-GC case — does not run.

## 6. Teardown wiring required (NOT yet applied)

`forget_vm_security_state(shared.vm_identity)` must be called from VM shutdown,
alongside whatever other per-VM native state is released. It is safe to call
more than once and safe to call for a VM that never installed anything.

## 7. What did not change

* Single-VM behaviour. One VM sees the same install / read / clear semantics,
  the same lazily-allocated default `Policy`, the same stable shared
  `Permissions` collection, and the same `Policy.isSet` answer.
* The lenient `setPolicy` / `setSecurityManager` contract (T19.H9): neither
  throws, and JEP 486's unconditional `UnsupportedOperationException` is still
  deliberately not adopted — see the rationale block on the
  `System.setSecurityManager` registration.
* The allow-all default when no `java.policy` is loaded, and the
  `CRATONVM_REQUIRE_POLICY` fail-closed override.
* `ACTIVE_POLICY` (the *parsed* `java.policy`), `ACTIVE_POLICY_PATH`, and the
  `REQUIRE_POLICY` / `DBG_DOPRIV` flag caches remain process-global. They hold
  no `ObjectRef`, so neither defect applies to them — but the parsed policy is
  still shared across VMs, which is a **separate open item**: two VMs cannot
  yet run under different `java.policy` files.
