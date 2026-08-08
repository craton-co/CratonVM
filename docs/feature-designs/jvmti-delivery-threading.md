# JVMTI event delivery and VM attribution

**Status:** Partial — interpreter delivery is fully VM-attributed; the C
`jvmtiEnv` surface a native agent would drive does not exist.

## What it does today

Events are fired **synchronously on the raising thread**, and every one of the
33 interpreter delivery sites passes both the thread id and the raising VM's
`vm_identity` (`vm/src/runtime/interpreter/jvmti_events.rs`). The
`UNATTRIBUTED_VM` (`0`) migration seam is no longer reachable from
`vm/src/runtime/interpreter.rs` or `vm/src/runtime/interpreter/**` on any path.

This is the delivery half of the JVMTI per-VM scoping work (see
[`../architecture/per-vm-state.md`](../architecture/per-vm-state.md)), which made
the environment registry per-VM (`ENVIRONMENTS`, keyed on `vm_identity`) but
could not thread the identity down to the sites that raise
events. Until it was threaded, every interpreter event in production landed in
the unattributed row rather than in the raising VM's.

The rest of the Rust-side plumbing is real: `vm/src/jvmti/mod.rs` (`JvmtiEnv`,
`ObjectTagMap`, heap iteration, `get_class_methods`, local-variable get/set),
`capabilities.rs`, `events.rs`, `agent.rs` (`dlopen` plus
`Agent_OnLoad`/`OnAttach`/`OnUnload`), `vm/src/runtime/jvmti.rs` (per-VM
`ENVIRONMENTS`, `JVMTI_VERSION_11`, field watchpoints, the `fire_*_for_vm`
family), and `-agentlib:` / `-agentpath:` parsing in `vm/src/config.rs`.

## What is not built yet

- **There is no C `jvmtiEnv` function table anywhere in the tree** — only a
  `typedef void* jvmtiEnv` in the generated header. Worse, `jni_get_env`
  (`vm/src/native/jni.rs`) **ignores the requested version and always returns
  the `JNIEnv*`**, so an agent calling
  `GetEnv(vm, &jvmti, JVMTI_VERSION_1_2)` receives a `JNIEnv`. A real native
  agent can be `Agent_OnLoad`'d but cannot then drive JVMTI over the C ABI.
- **Six sites outside the interpreter still use `UNATTRIBUTED_VM`**, and one is
  load-bearing: `SharedVm::new` must keep populating row 0 until they land.
  `install_manager`, `set_field_watchpoint` and `manager()` still resolve on
  row 0.

## Site table

`vm` column is what the site passes. "one-liner" = the call already had a
`SharedVm` and only the callee name changed.

### Direct sites (5) — one-liners, exactly as the handover said

| site | delivers | how the VM is obtained | done |
| --- | --- | --- | --- |
| `interpreter.rs:17576` (`getstatic`) | `FieldAccess` | `shared.vm_identity` | ✅ |
| `interpreter.rs:17734` (`putstatic`) | `FieldModification` | `shared.vm_identity` | ✅ |
| `interpreter.rs:18084` (`getfield`) | `FieldAccess` | `shared.vm_identity` | ✅ |
| `interpreter.rs:18576` (`putfield`) | `FieldModification` | `shared.vm_identity` | ✅ |
| `interpreter.rs:8810` (`pop_and_recycle_frame_with_reason`) | `MethodExit` on exception unwind | `shared.vm_identity` | ✅ |

### Helper sites (7 helpers, 28 call sites) — `vm: usize` threaded from callers

| helper (definition) | delivers | callers threaded | how the VM is obtained | done |
| --- | --- | --- | --- | --- |
| `fire_jvmti_exception_catch` (`interpreter.rs:12242`) | `ExceptionCatch` | 8 — `interpreter.rs:8450`, `:9224`, `:9328`, `:12068`, `:12180`; `invoke.rs:15653`, `:15703`, `:15746` | `shared.vm_identity` at each caller | ✅ |
| `fire_jvmti_method_exit_normal` (`:12320`) | `MethodExit` (normal return) | 8 — `:10292`, `:10333`, `:17488`, `:17499`, `:17510`, `:17521`, `:17532`, `:17545` | `shared.vm_identity` | ✅ |
| `push_frame_and_fire_entry` (`:12426`) | `MethodEntry` | 10 — `interpreter.rs:8408`, `:8672`, `:13804`, `:13992`, `:14438`; `invoke.rs:12124`, `:13673`, `:21593`, `:22794`, `:23340` | `shared.vm_identity` | ✅ |
| `fire_jvmti_frame_pop_if_requested` (`:12362`) | `FramePop` | 1 — `:8820` | `shared.vm_identity` | ✅ |
| `fire_jvmti_single_step` (`:12392`) | `SingleStep` | 1 — `:9399` | `shared.vm_identity` | ✅ |
| `fire_jvmti_method_entry` (`:12310`) | `MethodEntry` | 0 — no callers; `push_frame_and_fire_entry` inlines it | parameter, for any future caller | ✅ (dead) |
| `fire_jvmti_method_exit_exception` (`:12344`) | `MethodExit` (unwind) | 0 — `pop_and_recycle_frame_with_reason` inlines it | parameter, for any future caller | ✅ (dead) |

---

## Guards over-approximate; delivery does not

Rule 5 of the brief, checked at every site touched rather than assumed.

The `any_*_listener_active()` / `any_field_watchpoint_active()` calls are
process-wide **union** mirrors, recomputed on listener/watchpoint transitions
(`publish_union_listener_flags`, `jvmti.rs:2942`). They were left as they are.
That is correct, and the reason is worth stating precisely, because "make the
guard exact too" is the obvious-looking follow-up and it is the wrong change:
the guard is polled per opcode, and the exact form
(`any_*_listener_active_for_vm`) is a `HashMap` lookup under an `RwLock`.

What matters is that no site *delivers* on the strength of the union. Verified
by reading the whole path:

1. Every site's fire now goes through `runtime::jvmti::fire_*_for_vm`.
2. Each of those resolves `manager_for_vm(vm)` (`jvmti.rs:3023`) — the raising
   VM's own row.
3. The resolved manager then re-checks **its own** enable set before invoking a
   callback: `JvmtiEventManager::fire_method_entry` (`:1474`) and every sibling
   open with `if !self.is_event_enabled(kind, Some(thread)) { return; }`, and
   `is_event_enabled` reads that manager's `global_events` / `thread_events`.

So a VM with no agent that enters a helper body because *another* VM is
listening finds nothing to deliver to and returns. That asymmetry is pinned by
`a_union_guard_never_delivers_another_vms_event`.

One guard is a **bail-out** rather than a pre-filter and is also correctly left
as a union: `try_execute_cached_trivial_instance_getter`
(`invoke.rs:21730-21732`) refuses its shortcut if *anyone* might be observing.
Over-approximating there means declining an optimisation, which is safe; the
exact query would let a shortcut run in VM A while VM B watches nothing of A's.

---

## Seam-removal status

**Removed on every interpreter path.** After this change no code under
`vm/src/runtime/interpreter*` can reach `UNATTRIBUTED_VM`. The seam survives
only where a row genuinely cannot be named:

`manager_for_vm(vm)` still falls back to row 0 when `vm` has no row. For the
interpreter that fallback is now unreachable in practice — `SharedVm::new`
installs the per-VM row (`vm_init.rs:3284`) before any bytecode runs — but it
is deliberately kept, because the correct behaviour for a VM that somehow
executes before its row exists is "deliver to whoever registered globally",
not "drop the event".

### The nine VM-less `fire_*` free functions now have zero production callers

Confirmed by grep across `vm/src` and `crates/`: `fire_method_entry`,
`fire_method_exit`, `fire_single_step`, `fire_frame_pop`,
`fire_exception_catch`, `fire_field_access`, `fire_field_modification`,
`fire_field_access_if_watched`, `fire_field_modification_if_watched` are
reached only from `runtime::jvmti`'s own `#[cfg(test)]` block. They are
**kept deliberately** — the older T17.Δ tests exercise them and they document
the row-0 contract — but a new production call to any of them is a
regression, not a shortcut. If a future audit wants a mechanical gate, that
list is the one to gate on.

### What remains unattributed, and why

All six are in `vm/src/vm/vm_init.rs` — outside this lane's files. That file
was being edited by another lane while this was written, so locate these by
name rather than by line if the numbers have drifted.

| site | delivers | why it is still row 0 | shape of the fix |
| --- | --- | --- | --- |
| `vm_init.rs:3310` `class_load_adapter` | `ClassLoad` | bare `fn(u32, &str, u64)` installed into `cratonvm_classloading`'s process-global hook registry; no VM in scope and none obtainable | change the hook contract in another crate — an API change, not a re-keying |
| `vm_init.rs:3313` `class_prepare_adapter` | `ClassPrepare` | same | same |
| `vm_init.rs:3389` gc-start adapter | `GCStart` | same, via `cratonvm_gc::install_gc_start_hook` | same |
| `vm_init.rs:3392` gc-finish adapter | `GCFinish` | same | same |
| `vm_init.rs:3420` `fire_vm_init()` | `VMInit` | **no reason** — `vm.vm_identity` is in scope | one-liner, see below |
| `vm_init.rs:7405` `fire_vm_death()` | `VMDeath` | **no reason** — `self.shared.vm_identity` is in scope | one-liner, see below |

Because those six still resolve row 0 through `global_manager()` — an *exact*
lookup with no fallback — **`vm_init.rs:3301`'s `install_global_manager` call
must stay** until they are attributed. Removing it as "now redundant" is the
trap; the comment already in the file records that it was tried and dropped
every one of those events. This doc does not recommend removing it.

### Cross-file one-liners this lane could not make

Two, both `vm/src/vm/vm_init.rs`. The callee side landed here so each is a
single-token change:

```rust
// vm_init.rs:3420, in SharedVm::new
crate::runtime::jvmti::fire_vm_init_for_vm(vm.vm_identity);

// vm_init.rs:7405, in Vm's shutdown path — must stay BEFORE
// release_vm_native_state, which drops the row via forget_vm_jvmti_state
crate::runtime::jvmti::fire_vm_death_for_vm(self.shared.vm_identity);
```

`fire_vm_init_for_vm` / `fire_vm_death_for_vm` are new in
`vm/src/runtime/jvmti.rs`. **Note the behaviour change** this buys, and take it
deliberately: today an agent that registered on row 0 sees VMInit/VMDeath for
every VM in the process; afterwards it sees only its own VM's, which is what
JVMTI actually specifies. Landing these two makes VMInit/VMDeath consistent
with the 33 sites above; leaving them is not a correctness hole, only an
inconsistency.

---

## Tests

`vm/src/runtime/interpreter.rs`, `mod jvmti_delivery_scoping_tests`. They live
next to the helpers because that is the half `runtime::jvmti`'s own tests
cannot reach: those pin the registry, these pin that the interpreter hands the
registry a real identity.

| test | what fails without the fix |
| --- | --- |
| `method_exit_reaches_only_the_raising_vms_agent` | VM A's normal-return MethodExit was delivered to row 0, and any agent there — including VM B's, before the scoping lane — saw it |
| `push_frame_and_fire_entry_attributes_method_entry_to_its_vm` | the single frame-push chokepoint mis-attributes *every* MethodEntry in the VM |
| `frame_pop_reaches_only_the_raising_vms_agent` | FramePop mis-attributed; also pins that `NotifyFramePop` stays one-shot |
| `single_step_reaches_only_the_raising_vms_agent` | pins that the per-*thread* single-step gate survives the per-*VM* one; a fix that replaced rather than added would pass every other test here |
| `exception_catch_reaches_only_the_raising_vms_agent` | the one helper with neither a thread nor a VM in hand, hence the likeliest to be reverted |
| `a_union_guard_never_delivers_another_vms_event` | a helper that trusted the union flag instead of re-resolving the row — the exact failure mode rule 5 names |
| `field_watchpoints_do_not_alias_across_vms` | `class_id` is unique only *within* a VM, so a watch armed in A matched an unrelated field in B |

**Parallel safety.** Each test takes its own identity from `scoped_vm()`
(base `0x7100_0000`, distinct from `runtime::jvmti`'s `0x7000_0000`) — real
identities come from `NEXT_VM_IDENTITY`, a counter from 1, so small integers
are not safe to fake with in a binary that also builds real `SharedVm`s.

Registering a listener moves the process-wide union mirrors, which
`runtime::jvmti`'s tests assert are *false*. Those tests serialised on a
`global_test_lock()` private to their own `mod tests`, which a second test
module cannot name — a lock only one of two writers can reach is not a lock.
It is now `runtime::jvmti::jvmti_registry_test_lock()` at module scope
(`#[cfg(test)] pub(crate)`), with `mod tests`'s `global_test_lock()`
delegating to it, and it is poison-tolerant so one failing test does not
convert every later JVMTI test into a spurious failure that hides it.
