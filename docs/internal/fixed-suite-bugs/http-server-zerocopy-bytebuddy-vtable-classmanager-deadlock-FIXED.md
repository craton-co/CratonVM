# ZeroCopyIntegrationTests / ByteBuddy dynamic-class-generation hang — FIXED

Status: FIXED (branch `fix/httpserver-zerocopy-bytebuddy-race-0706b`)

Date observed: documented 2026-07-06 (prior session, in
`docs/known-issues/http-server-cluster-residuals.md`). Root-caused and fixed:
2026-07-06 (this session).

## Original symptom

`org.springframework.http.server.reactive.ZeroCopyIntegrationTests` hung
indefinitely (no output after early JVM startup log lines) on roughly
40-60% of standalone runs on the Azure Linux host, until an external
timeout killed the process. Confirmed pre-existing (reproduced identically
on an unmodified `dev` baseline binary in a prior session's A/B test).

A `--stack-dump-on-timeout` capture (Java-level only) showed the stuck
thread deep inside ByteBuddy's dynamic-class generation:
`net.bytebuddy.dynamic.scaffold.TypeWriter$Default.make` ->
`MethodDelegationBinder` -> `StackManipulation$Compound.apply` ->
`TypeList$Generic$AbstractBase.getStackSize` -> `AbstractList$Itr.next`,
triggered by AssertJ's `Assumptions.assumeThat(...)` lazily generating and
caching a ByteBuddy proxy class the first time it runs in a process.

## This session's reproduction

Confirmed the flakiness still reproduces on current `dev` (worktree off
`9f1db39d`): **9/15 hangs (60%)** in an initial batch on an idle-ish host,
rising to **17/17 (100%)** in a second batch run while the shared Azure
host was under heavy load (`load average` 90-100+ from ~50 concurrent
sessions) — consistent with a lock-contention bug whose trigger window
widens under scheduling pressure, not a fixed-probability race.

`net.bytebuddy.description.type.TypeList$Generic$AbstractBase.getStackSize()`
(disassembled from `byte-buddy-1.18.10.jar` via `javap`) is:
```java
public int getStackSize() {
    int size = 0;
    for (Iterator it = this.iterator(); it.hasNext(); ) {
        size += ((TypeDescription.Generic) it.next()).getStackSize().getSize();
    }
    return size;
}
```
`TypeList$Generic$AbstractBase extends java.util.AbstractList`, and
`iterator()` is the real-JDK JDK-inherited `AbstractList.iterator()`, which
returns the real, un-shadowed `java.util.AbstractList$Itr` inner class (NOT
`ArrayList$Itr` — CratonVM has no native special-case for `AbstractList$Itr`
or `TypeList$Generic$Explicit`'s own iterator; only its `size()`/`get(int)`
are natively shortcut, per `is_bytebuddy_method_token_native_override` in
`vm/src/runtime/interpreter.rs`). So the loop itself runs as ordinary
interpreted/JIT'd bytecode — ruling out a native-fallback field-slot
collision bug (the `ArrayList$Itr` `lastRet`/`cursor` collision fixed
earlier in `8a61e6f8` is a structurally different bug and was already
correctly ruled out by the prior session).

## Root cause: ABBA lock-order deadlock between `class_manager` and `vtable_manager`

Live `gdb -p <pid> -batch -ex 'thread apply all bt'` on an actually-hung
process (PID captured mid-run, NOT via the timeout-based stack dump — 3
snapshots taken 3 seconds apart, all three byte-for-byte identical, proving
a genuine block rather than slow progress) shows:

**Thread "main-vm" (the interpreter mutator thread), blocked wanting an
EXCLUSIVE lock:**
```
#0  syscall ()
#1  parking_lot::raw_rwlock::RawRwLock::lock_exclusive_slow ()
#2  cratonvm_vm::runtime::vtable::vtable_install_adapter ()
#3  cratonvm_classloading::class_manager::fire_vtable_install_hook ()
#4  cratonvm_classloading::class_manager::ClassManager::define_class_with_options ()
#5  cratonvm_classloading::class_manager::ClassManager::load_class ()
#6  cratonvm_vm::vm::vm_init::SharedVm::load_class_concurrent ()
#7  cratonvm_vm::runtime::interpreter::resolve_class_loader_aware ()
#8  cratonvm_vm::runtime::interpreter::execute_ldc ()
...
```

**Thread "Thread-13" (a worker thread), blocked wanting a SHARED lock:**
```
#0  syscall ()
#1  parking_lot::raw_rwlock::RawRwLock::lock_shared_slow ()
#2  cratonvm_vm::runtime::interpreter::execute_invokevirtual_vtable_fast ()
#3  cratonvm_vm::runtime::interpreter::execute_frame ()
...
```

(A third thread, "cratonvm-jit-co" — the background JIT compile worker —
was idle on a condvar wait, not implicated; a fourth was sleeping in a
reference-queue timeout, also not implicated.)

**Mechanism (confirmed by direct code reading, not just inferred from the
backtrace):**

- `SharedVm::load_class_concurrent` (`vm/src/vm/vm_init.rs:3542`) does
  `let mut cm_guard = self.class_manager.write(); let result =
  cm_guard.load_class(name);` — the `class_manager` write lock (`RwLock<
  ClassManager>`, L10 in `runtime::lock_order`'s hierarchy, "highest level
  / acquired first") is held for the ENTIRE `load_class` call. `load_class`
  -> `define_class_with_options` -> `fire_vtable_install_hook` (a `&mut
  self` method chain, so the borrow checker guarantees the write guard is
  still held) -> `vtable_install_adapter` (`vm/src/runtime/vtable.rs:768`)
  -> `manager.write().install_vtable(...)` (`vtable.rs:842`), where
  `manager` is `SharedVm::vtable_manager` (`Arc<RwLock<VtableManager>>`,
  the SAME `Arc` registered as the process-wide `GLOBAL_VTABLE_MANAGER`
  singleton). **Lock order: `class_manager` (write) -> `vtable_manager`
  (write).**

- `execute_invokevirtual_vtable_fast` (`vm/src/runtime/interpreter.rs:26392`,
  the "fast path 0" for `invokevirtual`/`invokeinterface`, consulted ahead
  of the per-thread invoke cache) took `shared.vtable_manager.read()` at
  line 26841 and, **while still holding that guard**, called
  `shared.class_manager.read()` at the (former) lines 26862-26867 to look
  up `declaring_name`. **Lock order: `vtable_manager` (read) ->
  `class_manager` (read).**

Two code paths acquiring the same pair of `parking_lot::RwLock`s in
opposite order is a textbook ABBA deadlock: a thread holding
`vtable_manager` (read) and blocked acquiring `class_manager` (read) can
permanently block a class-loading thread that holds `class_manager`
(write) and is blocked acquiring `vtable_manager` (write) — parking_lot's
writer-preferring fairness (documented elsewhere in this codebase, e.g.
`interpreter.rs` around line 24410, as the reason a nested same-thread
`class_manager` read self-deadlocks) means once the writer's request is
queued, it will wait indefinitely for every outstanding/future reader,
including one that itself can never finish because it's waiting on the
lock the writer holds.

Neither lock is tracked by `runtime::lock_order`'s L0-L10 enforcement
hierarchy — `vtable_manager` isn't in the table at all (only
`class_manager`, `native_methods`, `heap`, `ref_processor`, `monitors`,
`thread_registry`, `flight_recorder`, `cleaner_actions`, `native_memory`,
`jvm_thread`, `scratch` are), so nothing caught this at compile- or
runtime-checked-debug-build time.

ByteBuddy's dynamic-class generation (`TypeWriter$Default.make`) is a
uniquely good trigger because it is deeply recursive reflective/lambda
dispatch (the captured "main-vm" backtrace shows ~140 stacked frames of
`try_lambda_dispatch` / `invoke_or_native` / `execute_frame` cycles from
`Stream.forEach`/`ArrayList.forEach`-style callbacks) that both defines new
classes (hitting the write side) and does ordinary virtual dispatch (hitting
the read side) in tight succession from different threads, but the bug is
general — any concurrent class-loading + virtual-dispatch pair can hit it.

## Fix

`vm/src/runtime/interpreter.rs`, in `execute_invokevirtual_vtable_fast`
("Step 4"): extract the two cheap fields needed after the vtable lookup
(`declaring_class_id: u64`, `is_native: bool`) and the `resolved_method`
`Arc` while still holding the `vtable_manager` read guard, then **drop that
guard before acquiring `class_manager.read()`** for the `declaring_name`
lookup. No behavior change — the `class_manager` read is still done, just
after the `vtable_manager` guard's scope ends rather than nested inside it.
This matches the write-side order (`class_manager` outer, `vtable_manager`
inner) used everywhere else, closing the ABBA cycle.

```rust
let (entry_cached, entry_is_native, entry_declaring_class_id) = {
    let guard = shared.vtable_manager.read();
    // ... vtable/slot/entry lookup, early-returns on CacheMiss unchanged ...
    (cached, entry.is_native, entry.declaring_class_id)
};
// `guard` (vtable_manager read lock) is dropped here, BEFORE class_manager.read().
let declaring_name = shared
    .class_manager
    .read()
    .get_class(cratonvm_types::ClassId::new(entry_declaring_class_id as u32))
    .map(|c| c.name.to_string())
    .unwrap_or_else(|| entry_cached.class_name.to_string());
```

Confirmed via `grep` that this was the ONLY call site of
`shared.vtable_manager.read()` in the interpreter, so this single change
closes the cycle at its only crossing point.

## Verification

- **Before fix** (this session, current `dev`, worktree `9f1db39d`):
  9/15 hangs (60%) on a lightly-loaded host; 17/17 hangs (100%) on the same
  binary once the shared host's load average rose to ~90-100 from other
  concurrent sessions.
- **Live gdb capture** of a hung process: 3 snapshots 3 seconds apart,
  byte-for-byte identical thread states — confirms a genuine block, not
  slow progress (also cross-checked: `cratonvm-jit-co`, the background
  compile worker, was idle on a condvar, ruling out the
  `try_jit_compile_callee_slow` GC/lock-discipline hazard documented
  elsewhere in `interpreter.rs` as a contributing factor here).
- **After fix** (same worktree, rebuilt, same binary path): **20/20 runs
  clean, 0 hangs (0%)**, each completing in 8-14 seconds (vs. hitting the
  60-second external timeout before). All 20 runs report the identical,
  correct outcome: `found=4 succ=2 fail=0 skip=0 abort=2 status=OK`
  (matching the previously-documented normal successful shape — 2 pass, 2
  are assumption-skipped/aborted, 0 fail — so the fix introduces no test-
  outcome regression).
- Spot-checked one sibling class in the same suite
  (`ErrorHandlerIntegrationTests`) post-fix: `found=12 succ=12 fail=0
  skip=0 abort=0 status=OK`.

## Files changed

- `vm/src/runtime/interpreter.rs` — `execute_invokevirtual_vtable_fast`,
  reordered the `vtable_manager`/`class_manager` lock acquisition (see
  diff above). No other files changed.
