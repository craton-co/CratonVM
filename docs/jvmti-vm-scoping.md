# JVMTI is now one environment per VM — census, fixes, and what remains

**Status: 🟢 the four process-globals in `vm/src/runtime/jvmti.rs` are gone.**
Event delivery, the real-agent bridge, and the field-watchpoint table are all
keyed on `vm_identity`. What is left is a **migration seam**, not a global: a
small number of call sites outside this lane's files still fire events without
a VM identity, and those land in a reserved `UNATTRIBUTED_VM` (`0`) row. Every
one of them is listed below with the exact one-line change that closes it.

This is the item
[`vm-process-global-state-round-2.md`](vm-process-global-state-round-2.md) left
open with the instruction *"do not fix in isolation"*:

> A watchpoint hit in VM B would be delivered to the agent's callbacks through
> `GLOBAL_MANAGER` regardless of how the watchpoint map is keyed. Scoping
> `FIELD_WATCHPOINTS` alone would produce a subsystem that *looks* isolated in
> review and is not — worse than leaving it visibly global.

Round 2's framing was correct and is confirmed by reading. Both halves land
together here.

---

## Census

`vm/src/runtime/jvmti.rs` had exactly four `static`s (plus one `#[cfg(test)]`
serialisation mutex). `vm/src/jvmti/**` — `mod.rs`, `agent.rs`, `events.rs`,
`capabilities.rs`, 2,326 lines — has **zero**: all of its state hangs off
`JvmtiEnv`, which is owned per VM as `shared.debug.jvmti_env`
(`vm/src/vm/realms/debug_realm.rs:84`). That asymmetry is why the bug was in
this one file.

| static (old line) | holds | whose state | what a second VM observed | action |
| --- | --- | --- | --- | --- |
| `GLOBAL_MANAGER: OnceLock<Arc<JvmtiEventManager>>` (`:2737`) | every registered callback, every event-enable set, the seven `any_*_listener` fast-path flags, the event counters | **one** VM's — whichever called `SharedVm::new` first | VM B's `ClassLoad`/`GC`/`MethodEntry` were delivered to VM A's callbacks, carrying `ClassId`s and `ThreadId`s from VM B's id spaces. VM B's own listeners were unreachable — `install_global_manager` is `OnceLock::set`, so VM B's manager was constructed and dropped on the floor. | **Replaced** by `ENVIRONMENTS: RwLock<Option<HashMap<usize, VmJvmtiEnvironment>>>`, keyed on `vm_identity` |
| `REAL_AGENT_ENV_BRIDGE: OnceLock<Weak<SharedVm>>` (`:2800`) | one `Weak` to the VM whose `shared.debug.jvmti_env` receives the 9 bridged event kinds | one VM's | **two live VMs**: VM B's bridged events went into VM A's real env — a `-agentpath:` agent attached to VM A saw VM B's classes. **Sequentially**: after VM A is dropped, `OnceLock::set` from VM B *still* fails and the stored `Weak` no longer upgrades, so VM B's agent received **zero** bridged events for the life of the process. | **Replaced** by the per-row `bridge: Option<Weak<SharedVm>>` |
| `FIELD_WATCHPOINTS: RwLock<Option<HashMap<(u64, usize), FieldWatchpoint>>>` (`:3096`) | `(class_id, field_index) -> FieldWatchpoint`. No `ObjectRef`, no heap address | one flat table for the process; `class_id` is only unique **within** a VM | a watchpoint set in VM A matched an unrelated field of an unrelated class in VM B | **Moved** into `VmJvmtiEnvironment::watchpoints` |
| `FIELD_WATCHPOINTS_ACTIVE: AtomicBool` (`:3106`) | "is any watchpoint set" | process-wide | any VM installing a watchpoint put **every** VM's getfield/getstatic/putfield/putstatic on the slow path | **Kept, deliberately**, as a documented conservative union — see "Guards may over-approximate" below |
| `tests::global_test_lock`'s `LOCK: OnceLock<Mutex<()>>` (`:4365`) | a test serialisation mutex | n/a | n/a | benign; kept |

### Nothing here holds an `ObjectRef` or a heap address

Rule 5 of the brief (two-halves GC treatment) turned out not to apply to
anything that is live. Checked by reading:

* `FieldWatchpoint` is `{ class_id: u64, field_index: usize, access_watched:
  bool, modification_watched: bool }` — metadata only. No root source is
  needed and none was added; adding one would be noise.
* `JvmtiEnv`'s local-variable side table stores `LocalValue::Object(Option<u64>)`
  (`vm/src/runtime/jvmti.rs:710-716`) — that **is** a raw heap address, and it
  is neither scanned nor remapped. It is not a bug today because
  `runtime::jvmti::JvmtiEnv` has **no production constructor**: every
  `JvmtiEnv::new()` in the repository is in a `#[cfg(test)]` block, and the
  file's own D2 note already records that `GetLocalVariable*` reads a side
  table nothing writes. **If that table is ever wired to real frames, it needs
  both halves.** Same conclusion for `VmLocalVariableProvider`
  (`vm/src/jvmti/mod.rs:538`), whose only constructors are its own unit tests.
* `ObjectTagMap` (`vm/src/jvmti/mod.rs:274`) keys on raw object addresses and
  **already has both halves** — `sweep_dead` (`:308`) and `update_after_gc`
  (`:323`). Neither has a caller anywhere in the repository, and neither does
  `set_tag`/`get_tag`, so the table is never populated. It is per-`JvmtiEnv`,
  i.e. already per VM. Latent, correct by construction, not wired.

---

## What landed

### 1. `ENVIRONMENTS` — one row per VM

`vm/src/runtime/jvmti.rs`. A row is
`VmJvmtiEnvironment { manager: Option<Arc<JvmtiEventManager>>, bridge:
Option<Weak<SharedVm>>, watchpoints: HashMap<(u64, usize), FieldWatchpoint> }`,
keyed on `vm_identity` — monotonic from 1, never recycled
(`vm/src/vm/vm_init.rs:9`, `:1358`), so a stale key can never be re-observed by
a later VM. `0` is the reserved unattributed key, the convention round 1
established.

Two `Option`s are load-bearing and were both a bug in the first draft:

* **`manager: Option<...>`, not an eagerly-minted empty manager.** A row created
  by the bridge installer for a VM that never installed its own manager must
  resolve *through* the unattributed row. An empty manager sitting in the row
  would shadow the fallback and silently swallow every event.
* **`bridge: Option<Weak<...>>`, not `Weak::new()`.** `Weak::new().strong_count()
  == 0`, which is indistinguishable from "the VM died". Pruning on
  `strong_count() == 0` alone deletes live rows that simply have no bridge yet.
  `is_dead()` requires `Some(w)` *and* an expired `w`.

New API (all in `runtime::jvmti`):

| function | purpose |
| --- | --- |
| `install_manager_for_vm(vm, mgr)` | the entry point production wiring should use |
| `manager_for_vm(vm)` | `vm`'s manager, falling back to the unattributed row |
| `any_listener_active_for_vm(vm)`, `any_{method_entry,method_exit,single_step,frame_pop}_listener_active_for_vm(vm)` | exact per-VM guards |
| `fire_{method_entry,method_exit,single_step,frame_pop,exception_catch}_for_vm(vm, ..)` | exact per-VM delivery |
| `set_/clear_field_watchpoint_for_vm(vm, ..)`, `field_watchpoint_for_vm(vm, ..)`, `any_field_watchpoint_active_for_vm(vm)` | per-VM watchpoints |
| `forget_vm_jvmti_state(vm)` | teardown |
| `UNATTRIBUTED_VM` | the reserved `0` key |

`JvmtiEventManager` gained a `vm: usize` field, `new_for_vm(vm)`, and
`vm_identity()`. `new()` is now `new_for_vm(UNATTRIBUTED_VM)`, so it keeps
compiling and keeps its old meaning.

### 2. The bridge is per VM

`install_real_agent_env_bridge(&shared)` is the one production entry point that
already had an `Arc<SharedVm>` in hand, so it needed **no cross-file change**:
it now keys on `shared.vm_identity`. The nine bridged `fire_*` methods resolve
through `self.bridged_shared()`, which uses the manager's own `vm` — an
attributed manager can only ever reach its own VM's `shared.debug.jvmti_env`.

An *unattributed* manager falls back to `sole_live_bridge()`: the unique live
bridged VM, or `None` if there are zero or several. That is what fixes the
sequential case — after VM A is released, VM B becomes the sole live bridge and
starts receiving events, where the old `OnceLock` was stuck on a dead `Weak`
forever.

The stale comment at `vm/src/vm/vm_init.rs:6696` calls the old bridge
"idempotent, last-writer-wins shape as the two hooks just above". It was
`OnceLock::set` — **first**-writer-wins, unlike
`set_global_shared_vm_for_hooks` two lines above it, which is a real per-VM
registry. That mismatched comment is a good part of why this survived review.

### 3. Guards may over-approximate; delivery may not

`any_*_listener_active()` and `any_field_watchpoint_active()` are polled per
opcode. They now read process-wide **union** mirrors (`UNION_*`,
`FIELD_WATCHPOINTS_ACTIVE`), recomputed on every listener/watchpoint
transition — i.e. on agent attach/detach and `SetEventNotificationMode`, never
on an event. The union is:

* **never false while some VM is listening**, so no event is ever missed;
* **possibly true while the asking VM is not**, which costs one predicted
  branch and a per-VM re-check.

Every `fire_*` then resolves the exact VM. The brief's concern — "a process-wide
*is anyone listening* flag makes VM A pay VM B's event cost, and worse, can make
VM A *deliver* an event VM B's agent registered for" — is split: the cost is
accepted and documented, the delivery is fixed. The alternative for the guard is
a map lookup under a lock per getfield/putfield/invoke, which is not affordable.

The hot path is not slower than before: one `Acquire` load replaces
`OnceLock::get()` + a per-manager `Acquire` load.

### 4. Teardown

`forget_vm_jvmti_state(vm)` drops the row: the manager (and with it every
callback closure an agent registered), the bridge, and the watchpoints. It also
prunes rows whose bridge has provably expired, so `sole_live_bridge()` cannot be
poisoned forever by a VM that was torn down without the hook. Idempotent, for
the same reason `forget_vm_transformers` is: the hook it belongs in is invoked
from two places, either of which may run first or alone.

Rows are **moved out under the lock and dropped after it is released** — dropping
a row runs agent-supplied `Drop` code, which must not execute while this
module's registry lock is held. The hook is reached from `Drop for SharedVm`.

**This lane could not wire it.** See the cross-file change below.

---

## Cross-file changes this lane could not make

All four are one-liners. Ordering between them does not matter: the
unattributed-row fallback makes every intermediate state correct for a
single-VM run.

### A. Teardown — **required**, otherwise rows leak

`vm/src/vm/vm_init.rs:7288`, inside `release_vm_native_state`, next to the three
calls already there:

```rust
crate::runtime::jvmti::forget_vm_jvmti_state(vm_identity);
```

Without it a disposed VM's row survives for the life of the process: its
manager keeps the agent's callback closures alive, its listener flags keep every
*other* VM's interpreter on the slow path, and its watchpoints keep matching a
`class_id` that now means a different class. The opportunistic dead-row prune
limits the damage but only fires on the next registry write.

`vm/src/vm/vm_init.rs`'s
`dropping_a_vm_releases_its_capability_and_security_state` is the natural place
to assert it, mirroring what round 2 did for the transformer row.

### B. Per-VM manager at construction — closes the last delivery gap

`vm/src/vm/vm_init.rs:3283`, in `SharedVm::new`:

```rust
crate::runtime::jvmti::install_manager_for_vm(
    vm.vm_identity,
    std::sync::Arc::new(crate::runtime::jvmti::JvmtiEventManager::new_for_vm(vm.vm_identity)),
);
```

(replacing `install_global_manager(Arc::new(JvmtiEventManager::new()))`).

The comment above that call — "`install_global_manager` is idempotent so
repeated `SharedVm::new` calls in the same process (rare; mostly test
harnesses) are safe" — is exactly the assumption this workstream exists to
retire. It should be updated in the same edit.

### C. Interpreter call sites — two groups, and only one of them is a one-liner

**C1 — field watchpoints: genuinely one-line each.** All four sites have
`shared: &SharedVm` in scope (`vm/src/runtime/interpreter.rs:17032` resolves the
field through it two lines above the first one):

| file:line | from | to |
| --- | --- | --- |
| `vm/src/runtime/interpreter.rs:17046` | `fire_field_access_if_watched(..)` | `fire_field_access_if_watched_for_vm(shared.vm_identity, ..)` |
| `vm/src/runtime/interpreter.rs:17203` | `fire_field_modification_if_watched(..)` | `fire_field_modification_if_watched_for_vm(shared.vm_identity, ..)` |
| `vm/src/runtime/interpreter.rs:17550` | `fire_field_access_if_watched(..)` | `fire_field_access_if_watched_for_vm(shared.vm_identity, ..)` |
| `vm/src/runtime/interpreter.rs:18041` | `fire_field_modification_if_watched(..)` | `fire_field_modification_if_watched_for_vm(shared.vm_identity, ..)` |

Also one-line: `vm/src/runtime/interpreter.rs:8800`
(`fire_method_exit` inside `pop_and_recycle_frame_with_reason(shared, thread,
..)`, `:8790`).

**C2 — method-level events: NOT one-liners. Checked, and the brief's
assumption does not hold here.** Every other site is inside a private helper
that takes only `&JvmThread`/`&Frame` and has **no route to a `SharedVm`** —
`JvmThread` carries none, the same gap round 1 hit with
`VmLocalVariableProvider`:

* `fire_jvmti_method_entry(thread, frame)` (`:12245`)
* `fire_jvmti_method_exit_normal(thread, frame, rv)` (`:12255`)
* `fire_jvmti_method_exit_exception(thread, frame)` (`:12274`)
* `fire_jvmti_frame_pop_if_requested(thread, popped_by_exc)` (`:12291`)
* `fire_jvmti_single_step(thread, frame, saved_pc)` (`:12317`)
* `fire_jvmti_exception_catch(frame, handler_pc)` (`:12202`)
* `push_frame_and_fire_entry(thread, frame)` (`:12342`, `pub(crate)`)

Converting these means adding a `vm: usize` parameter to each helper and
threading it from their callers — ~15 call sites across `interpreter.rs`
(`:8408`, `:8668`, `:8809`, `:9380`, `:10273`, `:10309`, `:13308`, `:13496`,
`:13942`, `:16992`-`:17016`, …), most of which do have `shared`. That is a
mechanical but non-trivial change and it is squarely in another lane's file
this wave, so it is described rather than attempted. Until it lands these
events go to the unattributed manager — i.e. exactly where they go today.

The `any_*_listener_active()` guards at all of these sites can stay as they
are: they are the cheap union pre-filter, and the `_for_vm` call re-checks
exactly.

### D. `SetFieldAccessWatch` does not arm the interpreter — pre-existing, unrelated to scoping

Found while censusing. `JvmtiEnv::set_field_access_watch`
(`vm/src/runtime/jvmti.rs:2199`) and `set_field_modification_watch` (`:2227`)
write a **per-env `HashSet<FieldWatch>`** and never touch the watchpoint
registry the interpreter consults. `set_field_watchpoint` has no production
caller at all — every call is in a `#[cfg(test)]` block. So an agent that calls
`SetFieldAccessWatch` today gets `Ok(())` and no events.

This fails *closed* (silence, not wrong events), so it is out of scope for this
change, but it is a capability answered optimistically and belongs on the same
list as the D2 `GetLocalVariable*` gap. Wiring it needs a `vm_identity` on
`runtime::jvmti::JvmtiEnv`, which it does not have — that env is also
test-only-constructed today.

### E. `instrument.rs`'s `unique_vm()` comment is stale — a live collision hazard

`vm/src/runtime/instrument.rs:2073-2079` (round 2's test helper) says:

> Real identities are `SharedVm` addresses; these are small counter values,
> which no real VM can produce, so a test row can never shadow a production row.

Real identities have been small counter values from `NEXT_VM_IDENTITY` since
round 1 (`vm/src/vm/vm_init.rs:9`), so a transformer test's `vm = 1` **can**
collide with a real `SharedVm` built by a parallel test. The tests in this lane
use a `0x7000_0000` base for exactly that reason. Not this lane's file to edit.

---

## Tests

All in `vm/src/runtime/jvmti.rs`, `mod tests`. Each takes its own
`vm_identity` from `scoped_test_vm()` (base `0x7000_0000`, so it cannot alias a
real VM) and each takes the pre-existing `global_test_lock()` — they read and
reset the unattributed row and they move the process-wide union mirrors, both of
which the older block also touches.

| test | what fails without the fix |
| --- | --- |
| `watchpoints_are_per_vm` | VM B saw VM A's `(class_id, field_index)` watchpoint |
| `watchpoint_hits_reach_only_the_owning_vms_callbacks` | the flat map + single manager delivered VM A's watch hit to VM B's callbacks |
| `listener_flags_are_per_vm_and_the_union_is_only_a_guard` | pins the intended asymmetry: exact per VM, superset across the process |
| `events_are_delivered_only_to_the_owning_vms_manager` | one manager meant both VMs' callbacks fired |
| `an_unclaimed_vm_falls_back_to_the_unattributed_row` | pins the migration seam, so applying change C before change B cannot silence JVMTI |
| `forget_vm_jvmti_state_drops_everything_and_is_idempotent` | teardown, including that the union mirrors are recomputed rather than left latched |
| `a_dead_bridge_is_pruned_but_a_bridgeless_row_is_kept` | the `Option<Weak>` vs `Weak::new()` distinction — the first draft's bug |
| `bridge_is_per_vm_and_a_second_vm_is_not_silently_dropped` | builds two real `SharedVm`s: asserts the second VM gets its own bridge (old code: `None`), and that releasing the first does not take the second's bridge with it |

Two assertions were deliberately **weakened** for parallel safety, and this is
worth knowing before someone strengthens them again: other test modules in this
binary build real `Vm`s, and `Vm::new` registers a live bridge. So
`sole_live_bridge() == Some(x)` and any `registered_environment_count()` delta
are not assertable — the "exactly one live VM" precondition is not ours to
control. `sole_live_bridge().is_none()` **is** assertable while this test holds
two live VMs, because three or more live VMs still answer `None`.

---

## What was deliberately not done

* **No GC root source was added.** Nothing in these files holds a live
  `ObjectRef` or heap address on any reachable path — see the census. Adding a
  root source with nothing to scan would be a false signal to the next auditor.
* **The two JVMTI implementations were not unified.** `runtime::jvmti`'s
  `JvmtiEventManager` and `jvmti::EventManager` remain separate, with the D14
  bridge between them. That is the larger task the file header already names;
  this change scopes what exists rather than merging it.
* **The `classloading` and `gc` hook adapters were left unattributed.**
  `vm/src/vm/vm_init.rs:3291-3298` and `:3370-3377` install bare `fn` pointers
  into process-global hook registries in *other crates*
  (`cratonvm_classloading::install_class_load_hook`,
  `cratonvm_gc::install_gc_start_hook`). Those adapters have no VM in scope and
  cannot get one without changing the hook signature in another crate. Their
  events land on the unattributed manager, which is exactly where they land
  today. Framing for whoever takes it: it is the same shape as the `PROCESS_VM`
  item — an API change to the hook contract, not a re-keying.
