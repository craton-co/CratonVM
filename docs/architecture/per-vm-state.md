# Per-VM state versus process-global state

Status: partial. Written for the C2 review's P0 ("remove remaining
process-global runtime state") and P1 ("make process-wide policy state per-VM
or explicitly process-scoped").

Acceptance criterion under review:

> Two — and then 100 — VMs can be created concurrently with disjoint classes,
> flags, policies and shutdown order without contamination.

**That criterion is not met today.** This document records why, which state was
fixed in this pass, which state was made VM-keyed, and what still blocks it.

---

## 0. The two facts everything else follows from

**Fact 1 — `ClassId`s are allocated per VM and therefore collide across VMs.**

`classloading/src/class.rs:1125` allocates the next id as
`ClassId::new(self.classes.len() as u32)`, and each `SharedVm` owns its own
`ClassStore` (`SharedVm::classes.class_manager`). So `ClassId(7)` names a
different class in every VM. Any process-global map keyed on a bare `ClassId`
(or on a `u32`/`u64` derived from one) aliases across VMs. Unless the lookup
re-verifies its hit against the live class, that is silent wrong behaviour, not
a cache miss.

**Fact 2 — the GC's root registries are per VM.**

`var_handle_roots` lives in `SharedVm::mem`
(`vm/src/vm/realms/heap_realm.rs:66`), so `NativeContext::register_var_handle_root`
(`vm/src/vm/vm_exec.rs:8324`) writes into the *calling* VM's registry and
`read_var_handle_root` (`:8332`) reads only from it. Per
`docs/threading/objectref-concurrency-contract.md`, relocation is STW-only and
**every address holder must be rewritten through the pointer map**. An
`ObjectRef` parked in process-global storage that no VM's root set covers is
exactly the unrewritable holder that contract forbids.

`vm_identity` (`vm/src/vm/vm_init.rs:482`, allocated from `NEXT_VM_IDENTITY` at
`:3033`, exposed as `NativeContext::vm_identity()` at `vm/src/vm/vm_exec.rs:12169`
and wrapped as `VmId` in `native-api/src/capability.rs:82`) is the **one**
VM-identity notion. Everything below reuses it. Do not introduce a second one.
(Since interpreter round i1 wave 21 it is derived from the VM's class-store
layout domain, `vm_init::vm_identity_for_layout_domain` = domain + 1, so the
layout-domain token the heap and the classloading hooks use is the same
identity; `NEXT_VM_IDENTITY` is retired.)

---

## 1. The security-manager finding — CONFIRMED, and now **FIXED**

### Where it actually lives

The review brief located it at `vm/src/native/security_manager.rs`. That path
does not exist. The real file is **`native-builtins/src/security_manager.rs`**,
which was outside the edit scope of the pass that found this; the edit
specified in §5 has since landed there.

### The bug (as found)

```rust
// native-builtins/src/security_manager.rs:117
static SECURITY_MANAGER: Mutex<Option<(i32, ObjectRef)>> = Mutex::new(None);

// :140
pub(crate) fn get_security_manager(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let (key, cached) = (*SECURITY_MANAGER.lock().unwrap_or_else(|e| e.into_inner()))?;
    Some(ctx.read_var_handle_root(key).unwrap_or(cached))
}

// :148
fn set_security_manager(ctx: &mut dyn NativeContext, sm: Option<ObjectRef>) {
    let entry = sm.map(|obj| {
        ctx.register_var_handle_root(obj);
        (ctx.identity_hash_code(obj), obj)
    });
    *SECURITY_MANAGER.lock().unwrap_or_else(|e| e.into_inner()) = entry;
}
```

The slot is process-global; the root registration is per-VM (Fact 2). So with
VM A and VM B live:

1. `System.setSecurityManager(sm)` in VM A registers `sm` in **A's**
   `var_handle_roots` and writes `(key, sm)` into the process-global slot.
2. Any security check in VM B — `check_exec_or_throw` for
   `Runtime.exec`/`ProcessBuilder.start`, `check_host_native_access_or_throw`
   for `loadLibrary`/Panama (`:70`), `SecurityManager.<init>` (`:710`),
   `System.setSecurityManager` (`:971`) — calls `get_security_manager(ctx_b)`.
3. `ctx_b.read_var_handle_root(key)` looks in **B's** registry, misses, and
   `.unwrap_or(cached)` returns **VM A's raw `ObjectRef`**.

That returned reference is:

* **a foreign-heap address** — B then treats an object in A's heap as its own;
* **not a root of B** — B's collector never scans or rewrites it;
* **stale after A's next moving young GC or promotion** — A's collector rewrites
  its registry entry but cannot rewrite this raw static copy.

So it is a **use-after-move**, not merely a policy leak. The comment at `:108`
already documents the raw-copy hazard for the single-VM case and the
`read_var_handle_root` re-read is the mitigation — but that mitigation is
*defined only inside the owning VM*, so it silently degenerates to the
unmitigated raw copy in every other VM.

Additionally, and independently: `setSecurityManager(null)` from VM A writes
`None` into the shared slot, which **disarms VM B's `checkExec` and
`loadLibrary` gates**. Because `System.setSecurityManager` only consults the
*currently installed* manager for `RuntimePermission("setSecurityManager")`
(`:976`), an unsandboxed VM A can disarm a sandboxed VM B unchallenged.

### Regression test landed for it

`vm/src/vm/vm_init.rs`, test
`var_handle_roots_are_per_vm_so_a_global_cached_ref_cannot_be_remapped` pins the
underlying property: a var-handle root installed in VM A is not resolvable from
VM B, which is exactly what makes the `unwrap_or(cached)` fallback fire. It is
placed in `vm/src` because the fix must be made in `native-builtins`, and this
test fails the moment someone "fixes" the security manager by re-introducing a
process-shared registry rather than per-VM ownership.

### Two more instances of the identical shape in the same file

`ACTIVE_POLICY_OBJECT` and `SHARED_PERMISSION_COLLECTION` were byte-for-byte the
same `(identity_key, ObjectRef)`-in-a-process-global pattern, read back through
`ctx.read_var_handle_root(key).unwrap_or(cached)`. They had the same cross-VM
use-after-move, and any fix had to cover all three.

### What landed

All three are gone, replaced together:

* **Per-VM index.** `SECURITY_STATE: OnceLock<Mutex<HashMap<usize,
  VmSecurityState>>>`, keyed by `NativeContext::vm_identity()` — the same
  identity notion §0 Fact 2 insists on, not a second one. `VmSecurityState`
  carries all three slots; `security_slot` / `set_security_slot` take the key
  **from `ctx`**, so no call site can name another VM's row. Clearing the last
  occupied slot drops the row instead of leaving an all-`None` shell.
* **A real root source, which is what actually closes Fact 2.** Per-VM keying
  alone would have fixed the *policy* leak and left the raw cached `ObjectRef`
  still unrewritable. `gc_scan_security_manager_roots` /
  `gc_update_security_manager_refs` are registered as the `"security-manager"`
  source in `vm/src/memory/native_roots.rs` (`scan_security_manager` /
  `remap_security_manager`), so the cached copy is scanned as a root and
  rewritten through the pointer map — which is what
  `docs/threading/objectref-concurrency-contract.md` requires of every address
  holder.
* **A stated lock discipline.** The state lock is never held across a Java
  allocation or any re-entry into the VM, because the scan callback takes the
  same lock at a safepoint; the lazy initialisers allocate first and publish
  afterwards.

The regression test above still earns its place: it pins the *underlying*
property (a var-handle root installed in VM A is not resolvable from VM B), so
it fails the moment someone re-introduces a process-shared registry rather than
per-VM ownership.

**Still citing this as open, in code comments** (not edited here — this document
does not own `native-api/`): `native-api/src/capability.rs:15` and
`native-api/src/registry.rs:2087`. Both name `SECURITY_MANAGER` as a
process-global whose cached ref must be re-read through `read_var_handle_root`;
neither is true any more.

`ACTIVE_POLICY` (`:331`), `ACTIVE_POLICY_PATH` (`:344`), `REQUIRE_POLICY`
(`:57`) and `DBG_DOPRIV` (`:38`) are process-global **policy** state (P1 rather
than P0 — no `ObjectRef`): one VM's `java.policy` decides every VM's permission
checks, and `load_policy_file` from any VM rewrites it for all.

---

## 2. Inventory

Scope: process-global mutable state reachable from `vm/src/`. Severity classes:

* **CONTAMINATION** — two VMs produce *wrong* answers (aliased metadata, foreign
  `ObjectRef`s, misrouted events). Silent.
* **FIRST-WINS** — a `OnceLock`-shaped singleton the first VM latches; later VMs
  get the first VM's value or no service at all. Silent.
* **CONTENTION** — shared but self-verifying or content-addressed; two VMs evict
  each other and degrade, but no wrong answer.
* **PROCESS** — legitimately process-scoped (an OS resource, a signal handler, a
  diagnostic sink, an immutable table).

| # | State | file:line | Holds | Scope should be | Verdict | Fixed? |
|---|-------|-----------|-------|-----------------|---------|--------|
| S1 | `SECURITY_MANAGER` | `native-builtins/src/security_manager.rs:117` | `(i32, ObjectRef)` — installed `java.lang.SecurityManager` | per-VM | **CONTAMINATION** (use-after-move + gate disarm) | no — out of scope, §5 |
| S2 | `ACTIVE_POLICY_OBJECT` | same:205 | `(i32, ObjectRef)` — `java.security.Policy` | per-VM | **CONTAMINATION** | no — §5 |
| S3 | `SHARED_PERMISSION_COLLECTION` | same:216 | `(i32, ObjectRef)` — shared `Permissions` | per-VM | **CONTAMINATION** | no — §5 |
| S4 | `ACTIVE_POLICY` / `ACTIVE_POLICY_PATH` | same:331, :344 | parsed `java.policy` grants | per-VM | **CONTAMINATION** (policy, P1) | no — §5 |
| S5 | `REQUIRE_POLICY`, `DBG_DOPRIV` | same:57, :38 | fail-closed switch, debug switch | per-VM config | FIRST-WINS (P1) | no — §5 |
| V1 | `GLOBAL_VTABLE_MANAGER` | `vm/src/runtime/vtable.rs:934` | `Arc<RwLock<VtableManager>>` — vtables keyed by `ClassId` | per-VM | **CONTAMINATION** — second VM's vtables land in the first VM's index under colliding ids (Fact 1) ⇒ wrong virtual dispatch | **YES** (interpreter round i1 wave 19) — `SharedVm::new` binds its class manager's realm to `vm_identity`, registers its manager in a `vm_identity`-keyed registry (`register_vm_vtable_manager`) and installs the VM-scoped hooks (`vtable_install_adapter_scoped`); the global and the legacy hooks are no longer installed, and were deleted in wave 21 |
| V2 | `RESOLUTION_INVALIDATE_VM` | `vm/src/vm/vm_init.rs:3369` (was) | `Weak<SharedVm>` bridging the redefine / JIT-layout / class-info hooks | per-VM | **FIRST-WINS** — a second VM's redefines never invalidated its own `ResolutionCache`/`LinkResolver`/`JitCache`. Broken even *sequentially* (create A, drop A, create B) | **YES** — now a pruning registry of every live VM; adapters fan out |
| V3 | `object_class_id`'s `OBJECT_CLASS_ID` | `vm/src/vm/vm_exec.rs:1121` (was) | `ClassId` of `java/lang/Object` | per-VM | **CONTAMINATION** — first VM pinned its id process-wide; the stale-lambda-receiver check then compared VM B's receivers against VM A's id | **YES** — thread-local keyed `(vm_identity, ClassId)`, matching the two wrapper caches directly below it |
| V4 | `GATE_CACHE` | `vm/src/runtime/offload_jit_gate.rs:140` | `(ClassId, u16) -> bool` JIT-admission verdicts | per-VM | **CONTAMINATION** (Fact 1) — cross-VM verdict reuse could admit a method the analyzer would block | **YES** — key is now `GateKey = (usize, ClassId, u16)` |
| V5 | `ec_is_watched_class`'s `MEMO` | `vm/src/runtime/interpreter.rs:373` | `ClassId -> bool` | per-VM | **CONTAMINATION** (Fact 1), debug-gated (`CRATONVM_DBG_ECWATCH`) | **YES** — key is now `(vm_identity, u32)` |
| V6 | transformer chain `INSTANCE` | `vm/src/runtime/instrument.rs:102` | `Vec<TransformerEntry>`, each an **`ObjectRef`** (live `ClassFileTransformer`) | per-VM | **CONTAMINATION** — VM A's agent transforms VM B's class loads (Java-visible), and whichever VM collects scans/remaps the other's refs | no — §6 |
| V7 | `cache_registry` | `vm/src/runtime/serialization/oscache.rs:85` | `Vec<*const RwLock<FxHashMap<ClassId, ObjectRef>>>` — every VM's `ObjectStreamClass` cache | per-VM | **CONTAMINATION** — GC scan from one VM walks the other's maps; `ClassId` keys alias | no — §6 |
| V8 | `FIELD_WATCHPOINTS` | `vm/src/runtime/jvmti.rs:3096` | `(class_id u64, field_index) -> FieldWatchpoint` | per-VM | **CONTAMINATION** (Fact 1) — VM A's watchpoint fires on VM B's unrelated field | no — §6 |
| V9 | `GLOBAL_MANAGER` | `vm/src/runtime/jvmti.rs:2737` | `Arc<JvmtiEventManager>` | per-VM | **FIRST-WINS** — every VM's JVMTI events are delivered to the first VM's agent listeners | no — §6 |
| V10 | `REAL_AGENT_ENV_BRIDGE` | `vm/src/runtime/jvmti.rs:2800` | `Weak<SharedVm>` | per-VM | **FIRST-WINS** — same shape as V2, same fix available | no — §6 |
| V11 | `SET` (smuggled longs) | `vm/src/memory/smuggled_longs.rs:48` | `HashSet<u64>` of **heap addresses** minted as Java `long`s | per-VM | **CONTAMINATION** — `remap_and_sweep(pointer_map, &shared.mem.heap)` runs with ONE VM's heap; another VM's entries are not addresses in that heap, so the sweep **drops** them, after which that VM's genuine smuggled handles stop being rewritten across its own moves | no — §6 (needs `vm_identity` threaded through 8 call sites that have no `SharedVm`) |
| V12 | `PROCESS_VM` | `vm/src/native/jni.rs:323` | `Weak<SharedVm>` | process (by JNI design) | **FIRST-WINS**, last-writer-wins. `AttachCurrentThread` resolves "the" VM from an opaque `JavaVM*`; with two VMs a foreign thread attaches to whichever was created last | no — needs a `JavaVM*`→VM map, §6 |
| V13 | `ec_watch` table | `vm/src/runtime/ec_watch.rs:56` | `Vec<(ObjectRef, u32, usize, u32)>` | per-VM | **CONTAMINATION**, but entirely behind `CRATONVM_DBG_ECWATCH` | no — debug-only |
| V14 | `LAMBDA_SINGLETON_CACHE` | `vm/src/runtime/invokedynamic.rs:1284` | `(vm_identity, ClassId, pc) -> ObjectRef` | per-VM | **already correct** — the reference pattern for this whole document; scanned/remapped per VM from `native_roots.rs:202` | n/a |
| V15 | `padded_code_cache` | `vm/src/runtime/frame.rs:105` | `(class_id, sig_hash) -> Arc<[u8]>` | per-VM ideally | CONTENTION — every hit is re-verified against the actual bytes (`:148`), so a cross-VM key collision mints a fresh `Arc` | no — safe as-is |
| V16 | `method_slot_memo` | `vm/src/runtime/stackwalker.rs:132` | `(class_id, sig_hash) -> method index` | per-VM ideally | CONTENTION — every hit is re-verified against the live class (`:174`) | no — safe as-is |
| V17 | `local_liveness` `CACHE` | `vm/src/runtime/local_liveness.rs:74` | keyed by `Arc<[u8]>` allocation identity, guarded by `Weak::upgrade` + `Arc::ptr_eq` | — | CONTENTION only; cross-VM safe by construction | no — safe as-is |
| V18 | `force_native_over_real_jdk_bytecode_memoized` `CACHE` | `vm/src/runtime/interpreter/invoke.rs:9728` | `(class_name, method_name, descriptor) -> bool` | — | PROCESS — the wrapped predicate is pure over its arguments | no — correct |
| V19 | `JDK_ONLY_NATIVE_SHADOWS` / `JDK_ONLY_HELPER_VIOLATIONS` | `vm/src/vm/vm_exec.rs:230`, `vm/src/jit/helpers.rs:5949` | violation records for `--jdk-only-report` | per-VM ideally | CONTENTION — two VMs' reports merge into one | no — diagnostic |
| V20 | `ACTIVE_JDK_MODE`, `ACTIVE_GC_ALGORITHM`, `PRIMORDIAL_FRAME_TRACE` | `vm/src/runtime/crash_handler.rs:148, :152, :162` | crash-report config snapshot | process (one signal handler per process) | FIRST-WINS, but a crash report is inherently process-scoped; it will name the *first* VM's config | no — document, don't fix |
| V21 | `EVENT_LOOP_MANAGER`, `POOL`, `REG` | `vm/src/threading/event_loop.rs:840`, `threading/forkjoin.rs:244`, `threading/scheduled.rs:187` | shared worker pools that run Java code | per-VM | FIRST-WINS — VM B's tasks execute on VM A's pool threads | no — §6 |
| V22 | `CELLS` | `vm/src/threading/thread_state.rs:521` | `Vec<Arc<ThreadStateCell>>` — OS-thread state census | process | PROCESS — it is an OS-thread census, and an OS thread is a process resource. Correct as-is | n/a (file is out of scope by instruction) |
| V23 | `REGISTRY` (native root sources) | `vm/src/memory/native_roots.rs:94` | `fn` pointer pairs | process | PROCESS — the *table* is fine; what each callback reaches is the issue (V6/V7/V11) | n/a |
| V24 | `JNI_NATIVE_METHODS`, `DIRECT_BUFFERS`, `JNI_TABLE_PTR` | `vm/src/native/jni.rs:4730, :6279, :6881` | `RegisterNatives` bindings, direct-buffer addresses, the leaked JNI function table | process (JNI ABI) | PROCESS for the table; the binding map is keyed by a hash that embeds per-VM class identity ⇒ latent CONTAMINATION | no — §6 |
| V25 | `env_cache::MemoSlot` (≈25 sites) | `vm/src/runtime/env_cache.rs` | latched env-var-derived gates | process config | PROCESS-by-design, **P1 caveat**: the first VM latches; `VmConfig` differences between VMs are invisible to these gates unless `flags::overrides_active()` | no — see P1 note below |
| V26 | `FLAGS` | `types/src/flags.rs:1906` | the entire `VmFlags` set | process config | **FIRST-WINS** — this is the root of P1. Two VMs cannot have disjoint flags | no — out of scope, §5 |
| V27 | `BACKGROUND_COMPILER` / `BACKGROUND_COMPILER_INIT` | `jit/src/tiered.rs` (was) | the one compile-worker handle, started through a `std::sync::Once` against whichever manager asked first | per-VM | **FIRST-WINS** — a second VM's `ensure_background_compiler` was a no-op, so its queue was never drained and it never tiered up | **YES** (2026-09-12) — the handle lives on `TieredCompilationManager::background`; each manager starts its own C1 and C2 lane workers and joins them when dropped |
| V28 | `osr_deny_list` | `jit/src/tiered.rs` (was) | `HashSet<MethodKey>` of name-keyed OSR denials | per-VM | **CONTAMINATION** — one VM's (or one loader's) failed OSR compile denied OSR for every same-named method in the process, permanently | **YES** (2026-09-12) — `CompilerCore::osr_denied`, keyed by a loader-aware `MethodKey` and stamped with the install epoch |
| V29 | `JIT_BAIL_LIST`, `JIT_BAIL_REASONS`, `OSR_ENTRY_REJECTS` → `JIT_VERDICTS` | `jit/src/lib.rs` | negative compile verdicts keyed by (class, method, descriptor) names | per-VM ideally | CONTENTION — still process-wide (callers hold names, not a VM), so two VMs share verdicts about same-named methods; the worst case is a delayed compile | partial (2026-09-12) — unified into one store whose entries verify full names, expire with the redefine / install epoch and are cleared on unload and redefinition |

Counts: **34 entries** (S1–S5 + V1–V29).

* **14 CONTAMINATION** — S1, S2, S3, S4, V1, V3, V4, V5, V6, V7, V8, V11, V13,
  V28 (plus V24, latent).
* **9 FIRST-WINS** — S5, V2, V9, V10, V12, V20, V21, V26, V27.
* **5 CONTENTION** — V15, V16, V17, V19, V29.
* **6 PROCESS / already correct** — V14, V18, V22, V23, V24, V25.

Disposition of this pass:

* **Fixed (4):** V2 (moved to a live-VM registry), V3, V4, V5 (all three keyed
  by `vm_identity`). V3/V4/V5 are "keyed" fixes — the state stays in a
  process-global container, but a lookup can no longer cross VMs. V2 is a
  structural fix: the container itself now models "every live VM".
* **Comment corrected to state a real hazard (1):** V1 — the code is unchanged
  (the fix needs a `classloading` signature change, §5.3), but the comment no
  longer describes a soundness hole as a benign race.
* **Left as-is with a recorded reason (26):** everything else — 5 blocked on
  `native-builtins` / `classloading` / `types` (§5), 10 in-scope but too large
  or too hot-path for this pass (§6), 11 correct or contention-only.

---

## 3. What was fixed, and how

### V2 — redefine / JIT-invalidation / class-info hook bridge

`vm/src/vm/vm_init.rs`. Was `static RESOLUTION_INVALIDATE_VM: OnceLock<Weak<SharedVm>>`,
set from `Vm::new`. `OnceLock::set` silently ignores every call after the first,
so the **first VM ever created in the process owned the hook forever**. This was
wrong even without concurrency: create VM A, drop it, create VM B, and every
`RedefineClasses` in B fired a hook that upgraded A's dead `Weak` and returned —
leaving B's `ResolutionCache` and `LinkResolver` serving pre-redefine
`(declaring_class_id, index)` pairs, and B's `JitCache` holding code compiled
against a field layout that had since changed.

Now a `OnceLock<parking_lot::Mutex<Vec<Weak<SharedVm>>>>`.
`set_global_shared_vm_for_hooks` prunes dead entries and appends
(deduplicating by `Arc::ptr_eq`); `live_hook_vms()` snapshots the live VMs as
owning `Arc`s **and drops the registry lock before returning**, so the adapters
can take VM-internal locks without inverting the lock order.

The three adapters fan out to every live VM. This is sound *because the
operations are invalidations*: dropping a cache entry or retiring compiled code
in a VM that did not need it costs a re-resolve or a recompile, whereas failing
to invalidate your own is a correctness bug. The hook signature is `fn(u32)`
with no VM parameter, so over-application is the only available answer.
`class_info_adapter` is diagnostic-only and answers from the first VM that has
the id — its doc comment now says so explicitly, and warns that the name may
belong to another VM.

Single-VM behaviour: identical (one entry in the registry).

### V3 — `java.lang.Object`'s `ClassId`

`vm/src/vm/vm_exec.rs`. Was a process-global `OnceLock<ClassId>`. Now a
thread-local `Cell<Option<(usize, ClassId)>>` keyed by `shared.vm_identity` —
deliberately the same shape and key convention as
`PRIMITIVE_WRAPPER_CLASS_CACHE` / `NON_PRIMITIVE_WRAPPER_CLASS_CACHE`
immediately below it, which already carry the `(vm_identity, ClassId)` key and
whose comment already says "scoped to a specific `SharedVm` so sequential
in-process VMs cannot alias `ClassId`s".

The only consumer is the `stale_object_receiver` test in
`recover_stale_lambda_receiver_from_native_pins`, so the pre-fix failure mode
was silent in both directions: a genuinely-stale `Object`-typed receiver in the
second VM stops being recovered (spurious `NoSuchMethodError`), or an ordinary
receiver is misclassified as stale and the native-pin scan runs on every
virtual call.

Single-VM behaviour: identical, minus one process-global `OnceLock` read
replaced by a thread-local `Cell` read (if anything cheaper).

### V4 — GPU-offload JIT admission gate

`vm/src/runtime/offload_jit_gate.rs`. `(ClassId, u16)` → `GateKey = (usize,
ClassId, u16)` with `shared.vm_identity` in front. `#[cfg(feature =
"gpu-offload")]`, so this affects only GPU builds, but the dangerous direction
is real: a cached "does not block" verdict computed from VM A's bytecode could
admit a VM B method whose own bytecode the analyzer would have blocked.

### V5 — EC-watch class memo

`vm/src/runtime/interpreter.rs`. `u32` → `(usize, u32)`. Debug-gated
(`CRATONVM_DBG_ECWATCH`), fixed because it is a one-line instance of the exact
pattern and leaving it makes the pattern look optional.

### V1 — vtable manager: comment corrected, not fixed

`vm/src/runtime/vtable.rs`. The pre-existing comment said a second VM would
"lose the race" and that "whichever lands first owns the population stream".
That understates the hazard by a lot, and reads as an accepted limitation
rather than a soundness hole. Replaced with an accurate statement: because
`vtable_install_adapter` is a captureless `fn(u32, Vec<…>)` with no VM
parameter, the *second* VM's vtables are written into the *first* VM's manager
under colliding `ClassId`s, so `resolve_virtual_slot` can hand a caller in VM B
a `CachedBytecodeMethod` built from VM A's class. The comment also records why
the V2 fan-out trick does not apply here: invalidation is idempotent, vtable
*installation* is authoritative-state, and fanning it out would corrupt every
other VM.

---

## 4. Tests added

All in `vm/src/vm/vm_init.rs`'s `tests` module (integration tests under
`vm/tests/` were outside this pass's write scope). They serialize on a local
`HOOK_REGISTRY_TEST_LOCK` and assert only on the VMs they themselves create —
never on registry length — because the registry is process-global and other
tests share the process.

| Test | Asserts |
|------|---------|
| `vm_identity_is_unique_per_shared_vm` | The foundation every VM-keyed cache rests on. |
| `vm_keyed_class_ids_do_not_collide_across_vms` | The same numeric `ClassId` in two VMs produces two distinct keys and two distinct cache entries. |
| `var_handle_roots_are_per_vm_so_a_global_cached_ref_cannot_be_remapped` | **The security-manager evidence.** A var-handle root installed in VM A is unresolvable from VM B — so the `unwrap_or(cached)` fallback in a process-global singleton hands VM B a raw, unrooted, un-remappable reference into VM A's heap. |
| `hook_registry_reaches_every_live_vm_not_just_the_first` | V2 regression: a VM registered *after* another still receives hook callbacks; both adapters run without panic or lock inversion with two VMs live. |
| `hook_registry_registration_is_idempotent` | A VM registered three times appears once (otherwise every redefine invalidates it repeatedly). |
| `dropping_one_vm_leaves_the_other_registered` | Teardown isolation: the registry holds `Weak` (a dropped VM is not kept alive), the survivor stays registered, repeated sweeps are stable, and firing a hook after a teardown is a no-op rather than a panic. |

Not covered, and why:

* ~~*"Two VMs with conflicting security managers do not interfere"* and
  *"`setSecurityManager(null)` in one VM leaves the other's gates armed"* cannot
  be asserted until §5's fix lands.~~ **Now covered**, next to the fix in
  `native-builtins/src/security_manager.rs`:
  `two_vms_hold_independent_security_managers`,
  `clearing_one_vms_security_manager_leaves_the_other_armed`,
  `policy_and_permission_slots_are_per_vm`,
  `vm_teardown_does_not_disturb_the_other_vm`, and
  `single_vm_behaviour_is_unchanged` (the no-regression side).
* ~~*"The security-manager reference survives a forced young GC"* likewise
  belongs next to the fix, in `native-builtins`.~~ **Now covered** by
  `security_manager_ref_is_scanned_and_remapped_per_vm`, which is the test for
  the root-source half rather than the keying half.
* The var-handle test in the table above is still the right place for the
  *mechanism*: it fails if someone re-"fixes" this by sharing a registry rather
  than owning the reference per VM.

---

## 5. Required edits outside `vm/src/`

Listed in the order they must land.

### 5.1 `native-builtins/src/security_manager.rs` — S1/S2/S3 (P0, use-after-move) — **LANDED**

> **Done.** Both halves below are in the tree. The storage is a
> `vm_identity`-keyed `HashMap<usize, VmSecurityState>` rather than the
> `Vec<(VmId, …)>` sketched here — the same index shape as `VM_CAPABILITIES`,
> which is what this section asked for — and the `"security-manager"` root
> source is registered in `vm/src/memory/native_roots.rs` exactly as written
> below. The specification is kept because it is the template for the remaining
> process-global policy state in §5.2 onward.

Convert all three `(identity_key, ObjectRef)` process-global slots to
`VmId`-keyed storage and root them in the owning VM.

* `:117` `SECURITY_MANAGER`, `:205` `ACTIVE_POLICY_OBJECT`, `:216`
  `SHARED_PERMISSION_COLLECTION` → `Mutex<Vec<(VmId, i32, ObjectRef)>>` (or an
  `FxHashMap<VmId, (i32, ObjectRef)>`), using
  `cratonvm_native_api::capability::VmId::of(ctx)` — the existing per-VM
  registry pattern in `native-api/src/capability.rs:1189` (`VM_CAPABILITIES:
  OnceLock<Mutex<Vec<(VmId, Arc<CapabilitySet>)>>>`) is the model to copy, down
  to the `uninstall_*` teardown entry point at `:1221`.
* `:140` `get_security_manager(ctx)` → look up by `VmId::of(ctx)` first, then
  `ctx.read_var_handle_root(key)`. The `.unwrap_or(cached)` fallback then only
  ever fires inside the owning VM (mock contexts), which is what its comment
  already claims.
* `:148` `set_security_manager` → key the insert by `VmId::of(&*ctx)`.
* `:971` `System.setSecurityManager` → `null` must clear only the *calling* VM's
  entry. If a process-visible "no manager anywhere" state is ever wanted, it has
  to be a separate, explicitly-named process-scoped flag, not the absence of a
  per-VM entry.
* Add a per-VM teardown hook (mirroring `uninstall_capabilities`) so a destroyed
  VM's entry is removed rather than left holding a dangling address.
* Add `gc_scan_security_manager_roots(vm_identity, &mut Vec<ObjectRef>)` and
  `gc_update_security_manager_refs(vm_identity, &HashMap<usize, usize>)`
  alongside the existing `lang_math::gc_scan_value_of_cache_roots(vm_identity, …)`
  pair, so the reference is a first-class root of its owner rather than relying
  on the var-handle registry alone.

**Then, and only then**, add the `vm/src/` half — one entry in
`vm/src/memory/native_roots.rs`'s `VM_ROOT_SOURCES` table:

```rust
fn scan_security_manager(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::security_manager::gc_scan_security_manager_roots(
        shared.vm_identity, roots);
}
fn remap_security_manager(shared: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::security_manager::gc_update_security_manager_refs(
        shared.vm_identity, map);
}
// ...and in VM_ROOT_SOURCES:
root_source!("security-manager", scan_security_manager, remap_security_manager),
```

That half was deliberately not landed in the pass that wrote this — it cannot
compile until the `native-builtins` functions exist, and the workspace must stay
green. **Both halves have since landed together**, in that order.

### 5.2 `native-builtins/src/security_manager.rs` — S4/S5 (P1, policy)

`ACTIVE_POLICY` (`:331`), `ACTIVE_POLICY_PATH` (`:344`), `REQUIRE_POLICY`
(`:57`), `DBG_DOPRIV` (`:38`) → `VmId`-keyed. `set_active_policy` /
`load_policy_file` are `pub` and take no context; they need a `VmId` parameter
(or `VmId`-taking siblings, keeping the current names as
process-default-installing wrappers so no public item is removed).

### 5.3 `classloading` — V1 (P0, wrong virtual dispatch)

`VtableInstallHook` is `fn(u32, Vec<Option<VtableSlotDescriptor>>)`. It needs a
VM discriminator: either widen it to `fn(usize /* vm_identity */, u32, Vec<…>)`,
or have the classloading crate establish a current-VM thread-local around class
loading that `vm/src/runtime/vtable.rs:959`'s adapter can read. Then
`GLOBAL_VTABLE_MANAGER` becomes a `VmId → Arc<RwLock<VtableManager>>` map.
Until this lands, **two concurrent VMs can mis-dispatch virtual calls**, and
this is the single largest blocker to the acceptance criterion.

Landed in interpreter round i1 wave 19 through the scoped hook family that
already existed (`VtableInstallHookScoped`, which carries a `ClassHandle`):
the realm is bound in `SharedVm::new`, the managers are registered by
`vm_identity`, and the redefinition re-install goes through the scoped
dispatcher too. Interpreter round i1 wave 21 deleted `GLOBAL_VTABLE_MANAGER`,
the legacy `VtableInstallHook` / `VtableOverrideHook` family and
`ResolveError::ForeignVtableManager`, and derived `vm_identity` from the class
store's layout domain (`vm_init::vm_identity_for_layout_domain`), so the
realm's `VmId` and the hooks' owner token are one number.

### 5.4 `types/src/flags.rs:1906` — V26 (P1, the root of "disjoint flags")

`static FLAGS: OnceLock<VmFlags>` latches the first VM's flag set for the
process. The acceptance criterion explicitly names **disjoint flags**; it cannot
be met while this is a `OnceLock`. It needs to become `VmId`-keyed, with
`flags()` retained as a "process default" accessor for the many call sites that
have no VM in hand, and those call sites migrated incrementally. This is the
largest single piece of remaining work and was not attempted here.

---

## 6. Ordered remaining work

Ordered by *silent-wrong-answer risk first*, then by blast radius.

1. **S1/S2/S3 — security-manager and policy `ObjectRef`s** (§5.1). Use-after-move
   under a moving young GC, plus a cross-VM sandbox disarm. Highest severity:
   memory unsafety *and* a security-gate bypass.
2. **V1 — vtable manager** (§5.3). Wrong virtual dispatch between VMs. Needs a
   `classloading` signature change.
3. **V11 — smuggled-long registry** (`vm/src/memory/smuggled_longs.rs:48`). One
   VM's GC sweep silently drops another VM's minted handles, after which those
   handles stop being rewritten across that VM's own moves — a corrupted `long`
   or a stale reference. The fix is mechanical (key the set by `vm_identity`)
   but needs `vm_identity` threaded to eight call sites in `vm/src/native/jni.rs`,
   `vm/src/runtime/value_stack.rs` and `vm/src/jvmti/mod.rs` that currently have
   no `SharedVm` in scope. Not attempted here because that plumbing touches the
   value-stack hot path.
4. **V6 — `ClassFileTransformer` chain** (`vm/src/runtime/instrument.rs:102`).
   Holds live `ObjectRef`s *and* changes Java-visible behaviour: VM A's agent
   transforms VM B's class loads. Key by `VmId`; the existing
   `register_native_root_source` scan/remap pair must then filter by VM.
5. **V7 — `ObjectStreamClass` cache registry**
   (`vm/src/runtime/serialization/oscache.rs:85`). Same shape: per-VM maps,
   process-global registry, GC scan from any VM.
7. **V26 — `types::flags::FLAGS`** (§5.4) and **V25 — `env_cache` memo slots**.
   Required by the "disjoint flags" clause specifically. Large but mechanical.
8. **V12 — `PROCESS_VM`** (`vm/src/native/jni.rs:323`). Needs a `JavaVM*` → VM
   map so `AttachCurrentThread` resolves the right VM; today a foreign thread
   attaches to whichever VM was created last.
9. **V21 — shared worker pools** (`event_loop`, `forkjoin`, `scheduled`). These
   run Java code on threads owned by the first VM; correctness depends on how
   much VM state those threads touch. Needs its own audit before a fix.
10. **V19/V13 — diagnostic sinks.** `--jdk-only-report` merges two VMs'
    violations; `ec_watch` mixes two VMs' object addresses. Report-only. Fix
    last.

JVMTI (V8/V9/V10, `vm/src/runtime/jvmti.rs`) is **closed**: event delivery, the
real-agent bridge and the field-watchpoint table are all keyed on
`vm_identity`. What remains there is a migration seam, not a global — a small
number of call sites outside `jvmti.rs` still fire events with no VM identity
and land in a reserved `UNATTRIBUTED_VM` (`0`) row.

Every `ClassId`-keyed and call-site-keyed memo in `vm/src/jit/` carries a VM
key, and the one thread-local that caches raw heap addresses carries a heap
key.

Items deliberately **not** on this list because they are correct as-is: V14
(already VM-keyed and per-VM-rooted — the reference implementation), V15/V16
(self-verifying on hit), V17 (allocation-identity keyed), V18 (pure), V20
(a crash handler is genuinely process-scoped), V22 (an OS-thread census is
genuinely process-scoped), V23 (a table of `fn` pointers).

---

## 7. Rule of thumb for new code

Before adding a `static`, `lazy_static`, `OnceLock`, `OnceCell` or a
thread-local that outlives a VM, answer three questions:

1. **Does it hold an `ObjectRef`?** Then it must be owned per VM *and* reachable
   from that VM's root set through `VM_ROOT_SOURCES`
   (`vm/src/memory/native_roots.rs:339`) — both the scan and the remap half.
   Copy `LAMBDA_SINGLETON_CACHE` (`vm/src/runtime/invokedynamic.rs:1284`), not
   the security manager.
2. **Does it hold a `ClassId`, a method index, or anything derived from one?**
   Then it must be keyed by `vm_identity` (Fact 1), *or* every hit must be
   re-verified against the live class before it is used (the `frame.rs` /
   `stackwalker.rs` pattern).
3. **Is it a `OnceLock` that some VM installs at startup?** Then the second VM
   in the process gets the first VM's value. If that is not what you want, it
   needs to be a `VmId`-keyed registry with a teardown path — not a `OnceLock`.

And: there is exactly one VM-identity notion — `SharedVm::vm_identity` /
`NativeContext::vm_identity()` / `VmId`. Do not add a second.

---

## 8. JIT code memory and its bookkeeping (2026-09-12)

Not part of §2's counts. Compiled code is the one subsystem where most of the
state is process-scoped on purpose: executable pages are an OS resource, and an
OS thread can run compiled code belonging to more than one VM, so anything that
decides "may this body be freed?" has to see every thread in the process.

| # | State | Where | Holds | Scope | Verdict |
|---|-------|-------|-------|-------|---------|
| J1 | `JitCache`: shards, flush barrier, invalidation log, `generation()`, `redefine_epoch()` | `jit/src/lib.rs` (`JitRealm::jit_cache`) | compiled bodies by key, and "has this VM's cache changed?" | per-VM | per-VM. The generation the interpreter's negative memos compare against and the redefinition epoch inline caches stamp were process-global (`JIT_CACHE_GENERATION`, `REDEFINE_EPOCH`), so one VM's compile or redefinition flushed every VM's memos and inline caches. **FIXED**: both are fields of the cache. |
| J2 | `JIT_CACHE_GENERATION` | `jit/src/lib.rs` | count of every cache's publications and invalidations | process | PROCESS. Kept for the per-thread raw-entry dispatch memos in `vm/src/jit/helpers.rs` (`flush_raw_entry_dispatch_caches`): they live in thread-locals that outlive a VM and are keyed `(vm_identity, info)`, so a process count can only over-flush them. |
| J3 | `JIT_INSTALL_EPOCH`, `JIT_INVALIDATION_EPOCH` | `jit/src/lib.rs` | stamps taken when a compilation begins | process counter, per-cache gate | PROCESS. Monotonic counters: another VM's bump only makes a stamp older. The gates that compare against them (`flush_barrier`, the invalidation log) are per cache. |
| J4 | executable mappings, `COMMITTED_JIT_CODE_BYTES`, the code-cache cap | `jit/src/lib.rs`, `jit/src/platform.rs` | OS pages | process | PROCESS, with one CONTENTION: the cap is shared, so one VM's code counts against another's (`jit_code_cache_at_capacity`). |
| J5 | retirement queue (`DEFERRED_JIT_OWNERS`), `ACTIVE_JIT_EXECUTIONS`, `JIT_THREADS`, `JIT_RETIRE_GENERATION` / `JIT_GRACED_GENERATION`, reclamation counters | `jit/src/lib.rs` | owners awaiting grace; per-thread in-JIT records | process | PROCESS by necessity (a grace period must cover every thread that could be inside a body). |
| J6 | code-range registry, region list, `JIT_ENTRY_OWNERS`, compile-id table, `JIT_NAME_RANGES`, implicit-null ranges | `jit/src/lib.rs`, `jit/src/implicit_null.rs` | metadata for signal handlers, stack walkers and pointer validation | process | PROCESS. Keyed by code address (or a compile id bound to one), which is unique while mapped; every entry is withdrawn before its buffer is unmapped, and a released compile id is reissued only after grace. |
| J7 | `TYPECHECK_NAME_INTERN` / `TYPECHECK_TARGET_BY_SITE` | `jit/src/lib.rs` | leaked names keyed by `(name, ClassId)` | per-VM ideally | CONTENTION. A `ClassId` is per-VM (Fact 1), so `jit_typecheck_resolve` re-verifies that the recorded id names the site's class in its own VM before trusting it (the `frame.rs` pattern). Leaks one string per distinct pair. |

Rule for this subsystem: bookkeeping may be process-global only if it is keyed
by a code address that is unique while mapped, or if it is a monotonic counter
whose gate lives per VM. Anything that answers "has *this VM* changed?" belongs
on that VM's `JitCache`.

---

## 9. Interpreter round i1 wave 22 (lane L5)

A pass over the process statics that still hold per-VM facts, asking for each
what a two-VM process (two `cratonvm-embed` VMs, or two VMs created one after
another in one process) observes.

| # | State | Where | Verdict | Disposition |
|---|-------|-------|---------|-------------|
| W1 | JVMTI `ClassLoad` hook (`JvmtiClassHook`) | `classloading/src/class_manager.rs`, `vm_init.rs::class_load_adapter` | FIRST-WINS in effect: every VM's class loads were posted on the unattributed row, whose native-agent bridge answers nothing with two VMs live; a second VM's bootstrap loads reached the first VM's agent | **FIXED**: the hook carries the defining store's layout domain; `runtime::jvmti::fire_class_load_for_vm` (`docs/internal/fixed-bugs/interpreter-L5-classload-hook-carries-no-owner-token-FIXED-20260926.md`) |
| W2 | `JAVA_UTIL_CLASS_BITS` | `classloading/src/class_manager.rs` (was) | **CONTAMINATION** (Fact 1): a bitmap keyed by `ClassId`; one VM's `java/util/` class at id N made another VM's class N read as `java/util/`, which bars its receivers from the virtual door's promotion | **FIXED**: `JavaUtilClassBits`, one per class manager, read through `ClassRealm::java_util_classes` |
| W3 | `class_init_memo` `OWNER` + `BITS` | `vm/src/jit/helpers.rs` (was) | FIRST-WINS: correct (the owner latch), but every VM other than the first, including every VM created after the first was dropped, paid a class-manager read lock per compiled static access forever; `Relaxed` publication | **FIXED**: `ClassRealm::class_init_bits` (`ClassInitBits`), `Release`/`Acquire` |
| W4 | `ir::site_trap_decided_methods`, `ir::trapped_methods`, `ir_evidence::refused_methods` | `jit/src/ir.rs`, `jit/src/ir_evidence.rs` | **CONTAMINATION** by method name: a second VM's (or loader's, or redefined) copy of a method is never re-tiered after a site trap or a hot division guard | **FIXED** (wave 23): `ir::SiteTrapMemo` and `ir_evidence::IrRefusalMemo` are fields of each VM's `JitVerdictRegistry`, keyed by declaring class id (and the VM's redefine epoch); `docs/internal/fixed-bugs/interpreter-L5-ir-site-trap-decisions-are-process-wide-by-method-name-FIXED-20260926.md` |
| W5 | `ANY_CLASS_REDEFINED`, `ANY_DUPLICATE_CLASS_NAME`, `ANY_ANNOTATION_PROXY_DEFINED`, `CLASS_REDEFINITIONS` + its ring, `CLASS_DEFINITION_EPOCH`, `CLASS_NAME_GENERATION` | `classloading/src/class_manager.rs` | CONTENTION: one-way latches and monotonic counters that only over-approximate for another VM (a slower path or a re-resolve, never a wrong answer); the counters have per-store copies (`StoreEpochs`) for readers that can name their VM | no — correct as-is. (`ANY_ANNOTATION_PROXY_DEFINED` moved to the per-store `StoreEpochs::annotation_proxy_defined` in wave 25; see §10) |
| W6 | `CURRENT_VM_THREAD_ID` (thread-local) | `classloading/src/class_manager.rs` | per OS thread, not per (thread, VM): an OS thread registered as a Java thread in two VMs reports the last one's `ThreadId` in the other's `ClassLoad` events | **FIXED** (wave 23): `CURRENT_VM_THREAD_IDS`, `(layout domain, ThreadId)` pairs per OS thread; `set_current_thread_id(owner_domain, id)` at each registration, read with the defining store's domain |

---

## 10. Interpreter round i1 wave 23 (lane L5): every static in `vm/src` and `classloading/src`

**Method.** Every `static` declaration line in `vm/src/**` and
`classloading/src/**` (1401, `thread_local!` entries included), minus test
modules and `tests.rs` files, flag caches (`OnceLock` of a bool, number or
string), plain counters, constant tables (`JitInvokeInfo`, `NativeCallSite`,
arrays of literals) and `Once` / test locks: 493 candidates, each read for
WHAT it keys and WHAT it holds. A static holds a per-VM fact when its value or
key is a `ClassId`, a method or class name answered per VM, a VM epoch, or a
VM's address. Verdicts:

* **process-invariant** — the fact is the same in every VM (a pure predicate
  over names, an OS resource, a code address unique while mapped, a process
  counter used only as a monotonic stamp);
* **per VM** — keyed by `vm_identity` / layout domain / `HeapId` (never
  reused) or owned by a realm;
* **contention** — shared, but every hit is re-verified or only
  over-approximates (a slower path or a re-resolve, never a wrong answer);
* **bug** — two VMs (live together, or one after the other in one process)
  can observe a wrong answer.

### 10.1 Fixed this wave (each with a two-VM or sequential-VM test)

| # | State | Where | Was | Now |
|---|-------|-------|-----|-----|
| X1 | `ANNOTATION_PROXY_CID` | `vm/src/vm/vm_exec.rs` (was) | **bug**: a process `AtomicU32` holding VM A's `AnnotationProxy` `ClassId`; VM B's compiled virtual calls on a receiver of VM B's class with that id were routed into the annotation-proxy dispatch (`jit_invoke_virtual_mic`) | `annotation_proxy_cid_hint(shared)` reads the VM's own `ClassRealm::annotation_proxy_cid`; test `vm_init::tests::the_annotation_proxy_hint_is_per_vm` |
| X2 | `BOOTSTRAP_APPENDED_CLASSES` | `classloading/src/class_manager.rs` | **bug**: one VM's `appendToBootstrapClassLoaderSearch` made its class names bootstrap classes for every VM's user loaders (parent delegation before a `findClass` override; a miss not authoritative) | keyed by the appending VM's identity (`ClassManager::vm_id`); `is_bootstrap_appended_class(vm_identity, name)` at the nine `native-builtins` consumers; test `w23_l5_bootstrap_appended_tests`. Round 14 wave 2 (MISC11-2): no longer a static -- the `ClassManager` field `bootstrap_appended_classes`, read through `NativeContext::is_bootstrap_appended_class` |
| X3 | `CURRENT_VM_THREAD_ID` (W6) | `classloading/src/class_manager.rs` | **bug** (an embedder thread registered in two VMs) | `(layout domain, ThreadId)` pairs; test `a_thread_registered_in_two_vms_reports_each_its_own_thread_id` |
| X4 | site-trap sets, IR refusal memo, `JIT_PROCESS_REDEFINE_EPOCH` (W4) | `jit/src/ir.rs`, `ir_evidence.rs`, `lib.rs` | **bug** (keyed by method name; the process redefine epoch) | per `JitVerdictRegistry`; tests listed on the FIXED page |
| X5 | `CLASS_INITIALIZED_MEMO`, `TOTAL_FIELDS_CACHE`, `PRIMITIVE_WRAPPER_CLASS_CACHE` / `NON_PRIMITIVE_WRAPPER_CLASS_CACHE` (in `fast_unbox_primitive_wrapper` and its classified twin), `JNI_UNNAMED_MONITOR_OWNER`, `DIVISION_GUARD_TRAPS` (thread-locals) | `vm/src/vm/vm_util.rs`, `vm_exec.rs`, `native/jni.rs`, `jit/helpers.rs` | **bug**: keyed by the `SharedVm`'s ADDRESS, so a VM created where a dropped one lived inherited its answers for its own classes of the same ids (a class read "initialized" and its `<clinit>` was skipped; a non-wrapper's field 0 read as a boxed primitive). Their docs already claimed the `(vm_identity, ClassId)` convention | keyed by `vm_identity` (never reused); test `a_later_vm_does_not_inherit_a_dropped_vms_class_init_memo` |
| X6 | the gc crate's diagnostic namers | `gc/src/collector.rs`, `gc/src/gc.rs` | answered from "the first VM that has the id" | a heap that knows its layout domain asks `class_name_for_diagnostics_in` / `resolve_class_info_in` (the `GenerationalHeap` field-access and `ZgcRealHeap` guard reports); test `a_heap_diagnostic_names_the_class_of_its_own_vm` |

### 10.2 §2 rows re-checked (status on wave 22's `dev`)

| # | Now |
|---|-----|
| V6 transformer chains | **per VM**: `TransformerChains` / `LoadTimeOffered` are `vm_identity`-keyed maps (`runtime/instrument.rs`) |
| V7 `ObjectStreamClass` cache registry | **per VM**: no static is left in `serialization/oscache.rs` |
| V8 field watchpoints | **per VM**: only `FIELD_WATCHPOINTS_ACTIVE` (a one-way fast-path latch, contention) is process-wide; test `field_watchpoints_do_not_alias_across_vms` |
| V9 / V10 JVMTI manager and bridge | **per VM**: `ENVIRONMENTS`, a `vm_identity`-keyed registry (`runtime/jvmti.rs`) |
| V11 smuggled longs | **per VM**: `SETS` keyed by `HeapId`; `NONEMPTY` is a sound process-wide early-out |
| V12 `PROCESS_VM` | **bug, open (JNI ABI)**: `AttachCurrentThread` through the one process-wide invoke table still resolves "the" VM as the last created; `VM_REGISTRY` makes the ambiguity countable, and `process_vm_strict` / `vm_for_stw_flag_addr` refuse or resolve exactly where a guess is unsafe. Needs a `JavaVM*` per VM (an invoke table carrying its VM's identity) |
| V13 `ec_watch` | debug-only, unchanged |
| V14 `LAMBDA_SINGLETON_CACHE` | per VM |
| V15 / V16 / V17 | contention (verified hits), unchanged; the stackwalker's second memo (`class_id_memo`, name to id) is the same shape |
| V18 | process-invariant (a pure predicate over names) |
| V19 jdk-only violation reports | contention (diagnostics merge) |
| V20 crash-handler snapshot | process by nature |
| V21 worker pools | unchanged: `EVENT_LOOP_MANAGER` (virtual threads' event loops, process-unique loop ids) is shared; `forkjoin::common_pool` and `scheduled::registry` have no production reader in `vm/src` |
| V24 `RegisterNatives` bindings | **per VM**: moved into the native realm |
| V25 / V26 flags | unchanged (process config, §5.4) |

### 10.3 The rest of the candidates, by verdict

**Per VM** (the key carries `vm_identity`, a layout domain or a `HeapId`, or
the state lives in a realm): `GENERIC_INDY_SITES`, `SHARED_INDY_ENTRIES`,
`LAMBDA_PROXY_MARKERS`, `LAMBDA_PROXY_BRIDGES`, `USER_LOADER_LAMBDAS`,
`LAMBDA_SINGLETON_CACHE` (`runtime/invokedynamic.rs`); `PRIVATE_IMPL_MEMO`,
`RECEIVER_NATIVE_MEMO` (`interpreter/lambda.rs`); the `interpreter.rs` `MEMO`
(V5) and `multianewarray` plan cache; `GATE_CACHE` (V4); `VM_VTABLE_MANAGERS`;
`RESOLUTION_INVALIDATE_VMS` (the hook registry, keyed by layout domain);
`STORE_EPOCHS` (one slot per store); the `vm_identity`-keyed thread-local
memos of `jit/helpers.rs` (`CP_STAMP_CHECKED`, `LDC_SLOT_MEMO`, the wrapper
and `HashMap` class caches, the typecheck caches, `SITE_IDENTITY`,
`DIRECT_RECEIVER_MEMO`), `dispatch_static.rs` (`CLINIT_SITE_MEMO`,
`OWNER_RECHECK_MEMO`), `resolve/selection.rs` (`REAL_MEMO`, `SELECT_MEMO`,
keyed by the store), `vm_exec.rs` (`OBJECT_CLASS_ID`, the field and static
name rings), `vm_object.rs` (`STRING_LAYOUT_CACHE`); `ldc_global_slots`
(`(vm_identity, holder, cp index)`; since round 14 wave 2 (MISC11-1) no longer
a static but `NativeRealm::ldc_slots`); the `offload.rs` tables (keyed by VM, or
by code or buffer address); the unloaded-native-library tombstones (keyed by
realm address, cleared for every index a new realm hands out).

**Process-invariant:** the classloading hook `OnceLock`s (`fn` pointers; each
fire carries an owner token or a handle); `USER_LOADER_PARENTS` (loader
namespace ids come from one process counter, `NEXT_LOADER_ID`, so they never
collide across VMs; a dropped VM's entries are retained, never misread);
`type_maps::process_store` (the store for publishers with no VM; a VM's
verifier publishes into its own); the JIT's code-address tables and
`PROCESS_LIFECYCLE`; `LIVE_MONITOR_INDEXES` (object addresses are unique across
the process's heaps); the `native_override` `CACHE` (V18);
`thread_state::CELLS` (V22); the `env_cache` `MemoSlot`s (V25).

**Contention** (over-approximation or verified hits only):
`CLASS_REDEFINITIONS` and its ring (a record names a `ClassId` of SOME VM, so
a class-scoped reader in another VM over-retires; since wave 23 that includes
the `ResolutionCache` fill twins, where it costs one re-resolution),
`CLASS_ORIGIN_EPOCH`, `CLASS_DEFINITION_EPOCH`, `CLASS_NAME_GENERATION` and the
`ANY_*` latches (W5); `ir_evidence::INPUTS_GENERATION` (another VM's class
initialization re-offers this VM's input-stamped refusals: a rebuild);
`DOOR_RECV_MEMO` (keyed by class id and the caller's name `Arc` pointers,
which are per-VM allocations; a cross-VM hit would need a shared name
allocation and would only pollute a receiver profile); `FAST_THROW_SITES`
(`exceptions.rs`, keyed by the code pointer; its VM key was the `SharedVm`
address until wave 24, now `vm_identity`).

**Wave 24 (lane L5):** `vm/tests/per_vm_state_statics_ratchet.rs` freezes the
`static` count of `vm/src` (1331) and `classloading/src` (69) by the JIT
ratchet's rule, and fails on any `SharedVm` address cast to `usize` outside two
pointer-passing sites, so the next entry in this section arrives through
review.

**Wave 25 (lane L5):** the recount found `vm/src` at 1337: six statics came in
with the wave-24 merge beside the gate, reviewed and recorded in the gate's
`VM_BASELINE` note — `METHOD_ENTRY_REQUEST_VMS` (a count of VMs with a JDWP
`MethodEntry` request: a pre-filter, contention) and the thread-local
`ENTERED_FRAMES` (`debug/mod.rs`, wave 24 lane L1); `INFLATED_MONITOR_CACHE`
(keyed by monitor-table id and index epoch) and `BUDGET` (a flag cache) in
`threading/monitor.rs`; `NEW_CP_SITE_MEMO` and `MD_OURS_CLASS_MEMO`
(`vm_identity`-keyed thread-locals, `jit/helpers.rs`), from the `dev` merge.
Two contention latches then moved to per-VM owners, so one VM no longer
slows every other: `ANY_RESOLUTION_FAILURE_RECORDED`
(`interpreter/constants.rs`) is `ClassRealm::resolution_failure_recorded`, and
`ANY_ANNOTATION_PROXY_DEFINED` (`class_manager.rs`) is the per-store
`StoreEpochs::annotation_proxy_defined`. Baselines 1336 and 68.

**Scoped carriers** (set and consumed within one call on one thread; a nested
entry into another VM inside that window is not reachable):
`IN_PLACE_REDEFINITION` (`realms/jit_realm.rs`), `INVOKE_CP_IFACE_CID`
(`vm_exec.rs`), `PENDING_CLASS_HOOKS` (each queued event carries its owner
token). The defining-loader `IN_FLIGHT` guard (`constants.rs`, keyed by a bare
`(class id, name)`) was one too until interpreter round i1 wave 40 (lane L5)
moved it onto the thread as `JvmThread::loader_drives_in_flight`: the drive
runs a user loader's Java `loadClass`, which can reach a JNI native, and a
row left behind by a panic unwinding through the drive outlived its VM on the
OS thread, where a later VM's same-numbered class declined its own drive.
Baseline 1407.

**Diagnostics only** (merged across VMs by design): the `SEEN` dedupe sets,
the `HISTOGRAM` / `COUNTS` / census maps (`vm_exec.rs` `C` x3, the
`native_override` tallies, `jit_bridge` `CALLEE_PROBE_TALLY`, `osr_frame_trace`,
`exec_sampler`), `ROOT_ATTRIBUTION`, `SCAN_MARKS`, the `gc_and_alloc` trace
rings, the `JDK_ONLY_*` reports (V19), `define_census`.

Still open, in order: V12 (a `JavaVM*` per VM), V21 (the event-loop manager per
VM), V20 / V25 / V26 (process configuration by design, §5.4).
