# Capability layer — runtime install, dispatch gate, teardown

**Status:** the capability layer is **live at runtime**; enforcement is still off
by default (`CapabilityMode::Permissive`).
**Companion to:** `docs/security/native-capabilities.md` (§5 work list) and
`docs/security/capability-wiring.md` (the ~35 per-call-site gates).
**Scope of this pass:** `vm/src/vm/vm_init.rs`, `vm/src/vm/vm_exec.rs`, this file.
`native-api/`, `native-builtins/`, `native-io/` and `jit-api/` were not touched.

> This closes work-list items 2, 3 and 4 of `capability-wiring.md` §7. Before it,
> **every** gate in that document resolved `None`, because nothing in the process
> ever called `install_capabilities`. `capability_audit(vm)` answered `None` for
> every VM. The mechanism existed; it was not reachable.

---

## 1. Install, and why the ordering is the whole point

`SharedVm::new` (`vm/src/vm/vm_init.rs`):

```text
let vm_identity = NEXT_VM_IDENTITY.fetch_add(1, Relaxed);   // moved UP
let mut native_methods = NativeMethodRegistry::new();
let capabilities = Arc::new(CapabilitySet::from_env(VmId::from_raw(vm_identity)));
native_methods.set_capabilities(Arc::clone(&capabilities));  // ← before register_*
install_capabilities(Arc::clone(&capabilities));             // ← before register_*
native_methods.set_compatibility_mode(config.compatibility_mode);
    …the ~3,100-call `register_*` pass…
```

Three things had to move or be ordered, and each has a distinct reason:

1. **`vm_identity` is allocated at the top of `SharedVm::new`, not in the
   `Self { .. }` literal at the bottom.** The policy is keyed on it, and the
   policy has to exist before the first registration. `NEXT_VM_IDENTITY` is a
   monotonic counter, so the value and its uniqueness are unchanged — only its
   availability.

2. **`set_capabilities` precedes the `register_*` pass.** `register()` is itself
   a gate (`Capability::NativeRegister`), so a policy installed afterwards can
   never refuse a registration. It is also what populates the registry's
   `sensitive_slots` map (`slot -> CapabilityKind`, computed once from
   `classify_native`), which is what makes
   `NativeMethodRegistry::check_dispatch_capability` an integer lookup instead of
   a per-invocation string match. A late policy leaves all ~3,100 slots
   unclassified — the dispatch gate would be blind even under `Enforce`.

3. **`install_capabilities` publishes the *same* `Arc`**, into the process-wide
   `VmId -> CapabilitySet` index. That index is how a native holding only a
   `&dyn NativeContext` reaches its VM's policy (`CapabilityCheck`). One `Arc`
   means the registration gate, the ~35 per-call-site gates and the dispatch gate
   all write to **one** audit log. It is done at the earliest possible point so
   no boot-time native can run before the policy is visible — that window is
   exactly what `capability_gate`'s per-thread raw-memory memo would otherwise
   latch a stale `None` into.

### 1.1 The boot audit reset

The registration pass records one `native-register:<class>.<method>` row per
registered native: ~3,100 in real-JDK mode, ~5,200 with `synthetic-jdk`.
`MAX_AUDIT_ENTRIES` is **4,096**. In synthetic mode the audit map is therefore
*full before `main()` runs*, and every subsequent file/socket/spawn use — the
only thing the report exists to collect — is dropped.

So immediately after the registration pass, **and only under `Permissive`**,
`SharedVm::new` calls `capabilities.reset_audit()`. Nothing is lost: the same
rows, with registration provenance, are already in the registry's own census
(`registrations` / `provenance` / `census()`). `Audit` and `Enforce` keep every
row — in those modes an operator asked for the detail, and an `Enforce` run's
*denied* registrations are the actionable output. Those two modes can still hit
the cap in synthetic mode; see §5.

---

## 2. Teardown

Two hooks, and one idempotent function they share:

```rust
pub fn release_vm_native_state(vm_identity: usize) {
    uninstall_capabilities(VmId::from_raw(vm_identity));
    native_builtins::security_manager::forget_vm_security_state(vm_identity);
}
```

`forget_vm_security_state` had **no call site anywhere in the tree** before this.
Its row holds raw heap `ObjectRef`s (the installed `SecurityManager`, the policy
object, the shared permission collection), so per-VM security state was
outliving the heap that produced them.

| hook | fires when | covers |
|---|---|---|
| `impl Drop for SharedVm` (new) | the last `Arc<SharedVm>` dies | bare `SharedVm::new(..)` VMs — ~1,500 unit-test call sites across the workspace, none of which build a `Vm` |
| `impl Drop for Vm` (existing) | the owning `Vm` is dropped | the CLI, the test fixtures, and **`DestroyJavaVM`** — `libcratonvm::destroy_created_vm`, registered with `set_destroy_vm_hook`, takes the parked `Vm` and drops it |

`Drop for SharedVm` is the *precise* hook and would be sufficient on its own but
for one existing defect: `Vm::new` stores
`JcmdProcessor::new_with_vm_state(shared.clone())` into
`shared.debug.jcmd_processor` — a strong `Arc<dyn VmDiagnosticState>` pointing at
the `SharedVm` that owns it. A `Vm`-created `SharedVm` is therefore **never
dropped**. Until that cycle is broken, `Drop for Vm` has to carry the release.

Two consequences worth stating plainly:

* In `Drop for Vm` the release runs **last**, after the finalizer run and the
  JVMTI `VMDeath`: those still execute Java through natives, and a native that
  has just lost its policy reverts to allow-everything.
* Other threads may still hold `Arc<SharedVm>` clones and still be running Java
  when a `Vm` is dropped. From that point their gates resolve `None`, i.e. the
  permissive default. That is a fail-open **at shutdown**, and it is a
  consequence of the reference cycle, not of the ordering. Breaking the cycle
  makes `Drop for SharedVm` authoritative and removes it.

---

## 3. The dispatch gate

`vm/src/vm/vm_exec.rs`, `check_native_dispatch_capability`, called at the three
`find_with_kind` dispatch sites — each immediately before `safe_native_call`, and
**after** `resolve_native_dispatch_wave1` has committed to running the native
(those sites can still route the call to real bytecode; a capability is only
exercised by a native that executes).

This is the coarse safety net under the per-call-site gates. It can only report
`Scope::Any` — no arguments have been decoded — so an `Enforce` deployment must
hold the unscoped grant (`process-spawn:*`) for a class of native to dispatch at
all, and the per-call-site gate then makes the scoped decision. Its value is
coverage: it fires for every native in `classify_native`, *including those whose
implementation still has no gate of its own*.

### 3.1 The shape of the `Permissive` path

Three early-outs, in order:

| step | cost | answers for |
|---|---|---|
| 1. `registry.capabilities()` | one `Option` discriminant test on a field | a registry with no policy (embedder-built; `NativeMethodRegistry::new`) |
| 2. `classify_native(class, method)` | a `match` on `class_name` → a length switch plus a few `memcmp`s | the ~3,100 natives that are not capability-relevant — i.e. almost every dispatch |
| 3. thread-local memo, `Permissive` only | TLS load, two `usize` compares, one bit test | repeat dispatches of the few dozen that are |

**No allocation, no lock, no atomic, no hashing** on any of the three. In
particular no `NativeMethodId` is resolved on the common path — see §5 row 2 for
why that matters and what would remove step 2 entirely.

### 3.2 Why `Permissive` is memoized

`classify_native` maps **all** of `jdk/internal/misc/Unsafe` to `RawMemory` —
every `compareAndSetInt`, `getReferenceVolatile`, `park`, and every AQS / j.u.c
operation that lands in a native. `CapabilitySet::check` takes a
`parking_lot::Mutex` on the audit map, and that map is shared by every thread of
the VM. Recording per call would put a single VM-wide mutex on the hottest native
path in the interpreter — not a per-call instruction cost but a serialization
point, which in this codebase is how a throughput regression becomes a hang.

So under `Permissive` — and only under `Permissive`, which cannot refuse anything
— the first dispatch of each `CapabilityKind`, for each policy, on each thread
takes the full check; later ones take the memo. The memo is keyed on
`(vm_identity, policy address)`, so swapping a VM's policy or creating a second
VM invalidates it rather than inheriting a neighbour's answer.

The trade is the one `native-builtins`' `gate_raw_memory` already documents and
accepts (`capability-wiring.md` §4.4): the report still names the capability, its
scope and its first call site, and **under-reports `count`**. `count` is the one
number a least-privilege grant set does not depend on. `Audit` — the mode that
exists to price an `Enforce` flip — skips the memo and counts every call exactly,
and `Enforce` skips it because a refusal is a decision, not a counter.

### 3.3 What is *not* gated here

The JNI-function-pointer arm and the bytecode arm below the third site. A native
registered by a host library through `RegisterNatives` is not in
`classify_native`'s table, so the gate would be a guaranteed `None`; covering it
needs `RegisterNatives` itself gated, which is a separate row.

---

## 4. What now enforces, and what only records

| mode | registration | dispatch | per-call-site gates |
|---|---|---|---|
| `Permissive` (default) | allows, records (then reset — §1.1) | allows, records once per kind/thread | allows, records |
| `Audit` | allows, records + tallies ungranted | allows, records + tallies, every call | allows, records + tallies |
| `Enforce` | **refuses** ungranted registrations (nothing reaches `slots`) | **refuses** ungranted dispatch → `SecurityException` | **refuses**, with the scope |

Nothing in the default configuration refuses anything it did not refuse before.

---

## 5. Edits required outside this pass's scope

| # | file | edit | why |
|---|---|---|---|
| 1 | `native-api/src/capability.rs` | add a process-wide `AtomicUsize` install count and `pub fn any_capabilities_installed() -> bool` | **Recommendation: do it, but it no longer removes the memo.** See §5.1 |
| 2 | `native-api/src/registry.rs` (near `find_with_kind`, ~:5545) | add `find_with_kind_and_id(class, method, desc) -> Option<(NativeCallback, NativeKind, NativeMethodId)>` — the slot index is already in hand inside `slot_for_exact` | lets the three dispatch sites use `check_dispatch_capability(id)` (the precomputed `sensitive_slots` lookup) instead of `classify_native`, with no second triple hash. Would also let `record_native_dispatch`'s extra hash go |
| 3 | `native-api/src/capability.rs` | `MAX_AUDIT_ENTRIES` vs. `NativeRegister` | ~3,100–5,200 registration rows against a 4,096 cap. Either exempt `NativeRegister` from the per-entry map (a counter is enough; provenance already lives in the registry census) or size the cap above the registration count |
| 4 | `types/src/flag_groups.rs` (`SCALARS`) | declare `CRATONVM_CAPABILITY_MODE`, `CRATONVM_CAPABILITY_GRANTS`, `CRATONVM_CAPABILITY_LOG` | undeclared names are served by live `std::env` reads rather than the frozen `VmFlags` snapshot, so `-XX:` options cannot reach them |
| 5 | `jit-api/src/lib.rs:495` | `check_dispatch_capability(id)` beside `record_invocation(id)` | otherwise a JIT-dispatched native skips the net. An id **is** in hand there, so this one is the documented one-liner |
| 6 | `vm/src/vm/vm_init.rs` + `runtime/serviceability.rs` | break the `SharedVm` ↔ `JcmdProcessor` `Arc` cycle (store a `Weak`) | makes `Drop for SharedVm` authoritative, removes the shutdown fail-open in §2 |

### 5.1 On `any_capabilities_installed()`

The wiring doc (§4.5) asks for it to remove `gate_raw_memory`'s per-thread memo
and its staleness window. **The staleness half is now moot and the removal half
no longer follows**, for the same reason:

* **Staleness is gone anyway.** The memo's staleness window was "a policy
  installed *after* a thread has already taken a raw-memory path". With
  `install_capabilities` called during `SharedVm::new`, before any Java executes,
  no thread can observe a `None` that later becomes `Some`. The window closes
  because of the install ordering, not because of the atomic.
* **The fast path it was meant to enable is now the slow path.** The proposal
  was "one relaxed load, and the full check only ever runs when a policy actually
  exists". Every VM now installs a policy, so `any_capabilities_installed()` is
  `true` in every configuration and the flag would gate nothing. `gate_raw_memory`
  would fall through to a full `check` — a `String` allocation plus a VM-wide
  mutex — **per `Unsafe` byte access**. That is strictly worse than the memo.
* The `count` fidelity caveat in `capability-wiring.md` §4.4 therefore **cannot**
  be dropped, and the same trade is made here (§3.2) for the same reason.

It is still worth adding, for a different job: as a cheap *negative* answer for
call sites that today pay `capabilities_for` (a global mutex plus a `Vec` scan)
only to find nothing — chiefly embedder-built registries and `native-api`'s own
test contexts. Adding it is a small win; treating it as the fix for the raw-memory
gate is not.

---

## 6. Tests

`vm/src/vm/vm_init.rs`, `mod tests`:

* a booted VM has the policy in **both** places, and it is the same `Arc`;
* the boot default is `Permissive` with no grants, and the ~3,100-native
  registration pass still completes under it;
* the `Permissive` boot audit starts empty and un-truncated (§1.1);
* dropping a `SharedVm` uninstalls its policy **and** leaves no VM-scoped
  SecurityManager root behind;
* `release_vm_native_state` is idempotent (both hooks may run);
* two VMs hold independent sets — one records a use the other never sees — and
  dropping one leaves the other's installed.

`vm/src/vm/vm_exec.rs`, `mod tests` (each test builds its own VM, so it works
under its own `VmId` and cannot leak a policy into a parallel test):

* `Permissive` allows and records, and is behaviour-identical to no policy: it
  never refuses, and only the classified native reaches the audit log;
* `Permissive` records a kind once per thread and then goes transparent — and
  clearing the memo makes it record again (a memo, not a latch);
* `Audit` records and tallies **every** call without denying, and
  `suggested_grants()` hands back the grant that would fix it;
* `Enforce` denies an ungranted dispatch, repeatedly, as a `SecurityException`
  naming the capability — and leaves an unclassified native alone;
* `Enforce` admits a dispatch under an unscoped grant and refuses it under a
  scoped one (the gate's request is `Scope::Any`; fail closed);
* two VMs gate independently — an `Enforce` VM does not make its neighbour deny.

---

## 7. Related

* `docs/security/native-capabilities.md` — audit inventory, model, ordered plan.
* `docs/security/capability-wiring.md` — the per-call-site gates this net sits
  under, and §7's work list.
* `docs/security/per-vm-security-state.md` — the SecurityManager row that
  `forget_vm_security_state` releases.

