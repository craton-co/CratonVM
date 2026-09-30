# Foreign-thread attach with safepoint participation

**Status:** Shipped (default on; `CRATONVM_FOREIGN_ATTACH=0` restores the old
env-only stub).

## What it does today

A foreign — non-VM-created — OS thread is a first-class, GC-safe Java thread.
`AttachCurrentThread` / `AttachCurrentThreadAsDaemon` register it,
`DetachCurrentThread` reclaims it, and in between it participates in
stop-the-world through the existing barrier: a counted mutator while it is
running a call, idle-blocked between calls.

`attach_current_thread_impl` (`vm/src/native/jni.rs`) does real registration —
`attach_foreign_thread` / `detach_foreign_thread` build a `JvmThread`, register
it with the `ThreadRegistry`, publish its TLAB and thread address for
stop-the-world scanning, model an idle-attached thread as GC-blocked, and
refuse a detach with a call still in flight. The gate is
`foreign_attach_enabled()`, declared in `types/src/flag_groups.rs` under the
compatibility group as token `foreign-attach`.

A multi-threaded soak with K foreign threads plus a forced stop-the-world lives
in `libcratonvm`'s test module.

## Known limit

Resolution goes through the process-global `PROCESS_VM` cell
(`vm/src/native/jni.rs`), so **attach ignores which `JavaVM*` the caller
passes**. That is a real multi-VM limitation, not a diagnostic gap.

Related: [`embedding-api.md`](embedding-api.md) — the `JNI_CreateJavaVM` and
flat-C surface this builds on.

---

## 1. Problem & motivation

The JNI **Invocation API** lets native (host) code that owns its own OS threads
call into the VM:

```c
JavaVM *vm;  JNIEnv *env;
JNI_CreateJavaVM(&vm, (void**)&env, &args);   // on the bootstrap thread
...
// on a DIFFERENT, host-created thread:
(*vm)->AttachCurrentThread(vm, (void**)&env, NULL);
(*env)->CallStaticVoidMethod(env, cls, mid);   // run Java on this thread
(*vm)->DetachCurrentThread(vm);
```

This is the single most important capability for being a `libjvm` substitute:
servers, audio/render callbacks, thread-pool workers, and language runtimes all
call into Java from threads the JVM never created. Today CratonVM **cannot do
this safely**. A foreign thread that calls into Java:

1. has **no `JvmThread`** — no frame stack, no TLAB, no park state, no root
   snapshot, no shadow stack;
2. is **invisible to the `ThreadRegistry`** — it is not counted in
   `alive_count()`, has no `ThreadId`, no Java `Thread` object;
3. **does not participate in the stop-the-world (STW) barrier** — it neither
   polls `stw_requested` at safepoints nor deposits roots before blocking. A GC
   on another thread will either (a) deadlock because `request_stw`'s `expected`
   never matches what actually arrives, or (b) run a **moving** young-gen
   collection while the foreign thread holds live `ObjectRef`s in registers /
   Rust locals → use-after-free.

The result is that the *only* correct way to use CratonVM today is to call
everything from the one bootstrap thread. That is not an embedding API; it is a
single-threaded toy. This design closes the gap.

---

## 2. Current state in the codebase (what actually exists)

### 2.1 The Invocation API is a stub

`vm/src/native/jni.rs` builds an 8-slot invocation table
(`build_invoke_table`, `JNI_INVOKE_FUNCTION_COUNT = 8`) wiring:

* `t[3] = jni_destroy_java_vm` — returns `JNI_OK`, no-op.
* `t[4] = jni_attach_current_thread`
* `t[5] = jni_detach_current_thread`
* `t[6] = jni_get_env`
* `t[7] = jni_attach_current_thread` (reused as `AttachCurrentThreadAsDaemon`).

`jni_attach_current_thread` (jni.rs ~L5265) **ignores its `_vm` argument** and
only does this:

* if `JNI_SHARED_VM` TLS is already set, write the function table into `*penv`
  and return (treats "context present" as "already attached");
* otherwise write the function table into `*penv` and return `JNI_OK` —
  **without** setting `JNI_SHARED_VM`, **without** a `JvmThread`, **without**
  touching the `ThreadRegistry`.

The doc comment is explicit: *"full thread registration with the VM's
ThreadRegistry requires access to SharedVm which is obtained from the JavaVM\*
pointer."* That access does not exist yet — there is no `JavaVM* → SharedVm`
back-pointer, so the attach cannot reach the live VM. Consequently the
post-attach `JNIEnv` table calls that need a thread
(`with_jni_context`, jni.rs L241) silently get `None` (the `JNI_THREAD` TLS is
null) and return defaults.

`jni_detach_current_thread` clears `JNI_SHARED_VM` and `JNI_THREAD` TLS but,
having never registered anything, has nothing to deregister.

### 2.2 What a *real* VM thread looks like (the template to match)

`libcratonvm/src/lib.rs` `JNI_CreateJavaVM` (L397) is the one place that wires a
thread up correctly. It:

* builds `Vm::new(config)` (which constructs the **main** `JvmThread` and
  registers it as `ThreadId(0)`), runs `bootstrap`,
* captures `shared = vm.shared.get_arc()` and publishes it with
  `set_jni_context_arc(shared)` (jni.rs L162) into the bootstrap thread's TLS,
* hands back the process-global `get_java_vm()` / `get_jni_env()` tables,
* parks the `Vm` in the `CREATED_VM: Mutex<Option<ParkedVm>>` static for the
  life of the process.

`vm/src/vm/vm_init.rs` (~L4420-4445) shows the full per-thread wiring the main
thread gets, all of which a foreign thread currently lacks:

* `thread_registry.set_interrupted_flag(tid, jvm_thread.interrupted.clone())`
* `thread_registry.set_park_state(tid, jvm_thread.park_state.clone())`
* `thread_registry.set_root_snapshot(tid, jvm_thread.root_snapshot.clone())`
* `thread_registry.set_gc_block_state(tid, jvm_thread.gc_block_state.clone())`

### 2.3 `JvmThread` — the per-thread state a foreign thread needs

`vm/src/threading/jvm_thread.rs`. `JvmThread::new(ThreadId, name)` builds:
`frames`, locals/stack pools, `interrupted: Arc<AtomicBool>`, `park_state:
Arc<ParkState>`, `root_snapshot: Arc<Mutex<Vec<ObjectRef>>>`, `frame_trace`,
`gc_block_state: Arc<GcBlockState>`, `native_pin_roots`, `tlab:
Tlab::empty()`, `shadow_stack: ShadowStack::empty()`, `kind: Platform`. These
`Arc`-shared fields are exactly the ones the registry mirrors so a GC initiator
on another thread can scan/maintain this thread's roots.

The JIT also reads `JvmThread::tlab_offset()` and `shadow_stack_offset()` to emit
inline bump-allocation and shadow-stack pushes; a foreign thread's `JvmThread`
must therefore be a *real, stable* `JvmThread` (its address baked into JIT'd
code while the thread runs), not a transient stack temporary.

### 2.4 The STW barrier — how participation works

`vm/src/threading/gc_barrier.rs` `GcBarrier`:

* `stw_requested: AtomicBool` — cheap flag polled at safepoints.
* `request_stw(initiator, alive_count)` (L93): sets `expected = alive_count - 1
  - threads_blocked`. **`alive_count` comes from
  `ThreadRegistry::alive_count()`** (thread_registry.rs L736 — counts entries
  whose `alive` flag is set). A foreign thread that is *not* registered is not
  counted, so `expected` is computed as if it does not exist — yet it is running
  Java and holding live oops. This is the core unsoundness.
* `arrive_and_wait(tid)` (L242, counted) / `arrive_and_wait_excluded(tid)`
  (L258, blocked-but-must-wait) — what a participating thread calls when it
  observes `stw_requested`.
* `enter_blocked()` → `BlockedGuard` (L144) and `mark_blocked_region_{enter,
  leave}` (L171/L192): the blocking-native protocol. A thread about to park
  (`Object.wait`, `park`, `join`, selector `select`, blocking I/O) deposits its
  roots, increments `threads_blocked`, and is *excluded* from `expected`; the GC
  maintains its parked roots via `GcBlockState`.

`GcBlockState` (jvm_thread.rs L68): `in_blocked_region: AtomicBool` + `fixup`
map. While a thread is blocked, `ThreadRegistry::fold_pointer_map_into_blocked`
(thread_registry.rs L669) remaps its deposited `root_snapshot` in place and
chains frame fixups across every missed (possibly moving) GC; the thread applies
them on wake in `check_post_block_gc`.

`gc/src/gc_quiescence.rs` is a *process-wide* JIT-active counter
(`enter`/`leave`/`is_active`) the collector consults to fall back to a
non-moving sweep while any thread is inside JIT code. It is per-process, so a
foreign thread inside JIT is already covered for *quiescence* — but the
collector still needs that thread's roots and STW arrival, which it does not get
today.

`gc/src/safepoint.rs` is **not** the mutator safepoint — it is the
`gpu-offload`-feature `SafepointToken` RAII GC-defer guard. The real mutator
safepoint poll is the interpreter's `safepoint_check` (the loop that reads
`stw_requested`, deposits roots, and calls `arrive_and_wait`) plus the
blocking-site protocol above.

### 2.5 The JNI TLS context

`vm/src/native/jni.rs` thread-locals (L87): `JNI_SHARED_VM:
RefCell<Option<Arc<SharedVm>>>` and `JNI_THREAD: Cell<*mut ()>` (erased
`*mut JvmThread`). `set_jni_context_arc` / `clear_jni_context` and
`set_jni_thread` / `clear_jni_thread` manage them. `with_jni_context` (L241)
requires **both** set to produce `(&SharedVm, &mut JvmThread)`. A correct attach
must set both; detach must clear both.

### 2.6 Summary of the gap

| Capability | Main / Thread.start thread | Foreign attach today |
|---|---|---|
| Has a `JvmThread` (frames, TLAB, shadow stack) | yes | **no** |
| Registered in `ThreadRegistry` (counted in `alive_count`) | yes | **no** |
| `JNI_SHARED_VM` + `JNI_THREAD` TLS set | yes | partial (env only) |
| Polls `stw_requested` / `arrive_and_wait` | yes | **no** |
| Deposits roots at blocking sites | yes | **no** |
| `JavaVM* → SharedVm` reachable | n/a | **no back-pointer** |
| Clean detach (deregister, drop TLAB) | n/a | **no** |

---

## 3. Proposed design

### 3.1 Make `JavaVM*` resolve to the live VM

`AttachCurrentThread` receives `JavaVM*` but must reach the `Arc<SharedVm>`. Two
options; we take **(a)** for the first cut and keep **(b)** as the principled
follow-up:

* **(a) Process-global lookup (chosen first).** There is exactly one VM per
  process (`CREATED_VM` in `libcratonvm`, mirrored by the leaked
  `JNI_INVOKE_TABLE_PTR` singleton). Add a process-global
  `Weak<SharedVm>`/`Arc<SharedVm>` cell, published by `JNI_CreateJavaVM`, that
  the attach path upgrades. This is sound because the `JavaVM*` we hand back is
  itself a process-global singleton, so "the JavaVM\*" and "the one VM" are the
  same fact. Lives in `jni.rs` next to the table statics
  (`set_process_vm(Arc<SharedVm>)` / `process_vm() -> Option<Arc<SharedVm>>`).
* **(b) Real back-pointer (later).** Replace the opaque `JavaVM = *const *const
  usize` table with a small struct whose first word is the JNI invoke table
  pointer (so the ABI `(*vm)->Fn` indirection still works) and that also carries
  a `*const SharedVm`. More faithful to multi-VM hosts, but unnecessary while we
  are one-VM-per-process; deferred.

### 3.2 Attach: build and register a foreign `JvmThread`

`jni_attach_current_thread` becomes (pseudocode, real impl is the deliverable):

```
fn attach(daemon: bool, penv) -> JInt {
    if JNI_SHARED_VM is set { write env; return JNI_OK; }   // re-attach no-op (kept)
    let shared = process_vm()?;                              // §3.1
    let tid = shared.thread_registry.next_thread_id();
    // Box the JvmThread so its address is stable for the JIT (tlab/shadow offsets)
    // and for the JNI_THREAD raw pointer that outlives this call.
    let mut jt = Box::new(JvmThread::new(tid, &name_for(tid)));
    jt.kind = ThreadKind::Platform;
    jt.daemon = daemon;
    // Mirror the shared Arc fields into the registry (same as vm_init L4420-4445).
    shared.thread_registry.register_with_daemon(tid, &jt.name, None, daemon);
    shared.thread_registry.set_interrupted_flag(tid, jt.interrupted.clone());
    shared.thread_registry.set_park_state(tid, jt.park_state.clone());
    shared.thread_registry.set_root_snapshot(tid, jt.root_snapshot.clone());
    shared.thread_registry.set_gc_block_state(tid, jt.gc_block_state.clone());
    // Optionally create a java.lang.Thread object and set java_thread_obj so
    // Thread.currentThread() works (see §3.5); deferred to a later increment.
    set_jni_context_arc(shared);
    set_jni_thread(Box::into_raw(jt));   // ownership parked in TLS until detach
    write env into *penv;
    JNI_OK
}
```

Key points:

* The `JvmThread` is **heap-boxed and owned by the attaching thread's TLS**
  (`JNI_THREAD` holds the raw pointer; a parallel TLS `Box` keeps it alive). It
  must not move — the JIT bakes `tlab_offset`/`shadow_stack_offset`-relative
  addresses while the thread runs.
* Registration order matters for the GC race: register + share `root_snapshot` /
  `gc_block_state` **before** publishing `JNI_SHARED_VM`, so the first instant
  this thread can run Java (and trip a safepoint) it is already STW-visible with
  a (possibly empty) deposited snapshot.

### 3.3 Safepoint participation

Once registered, a foreign thread participates **exactly like a `Thread.start`
worker**, reusing the existing machinery — no new barrier code:

* **Running mutator poll.** Every Java call entered through the foreign thread
  runs the interpreter, which already polls `stw_requested` at allocation sites
  and backward branches and calls `deposit_root_snapshot` + `arrive_and_wait`.
  Because the thread is now in `alive_count`, `request_stw` includes it in
  `expected` and `wait_for_all` correctly waits for its arrival.
* **Pure-native foreign thread (between calls).** A foreign thread that has
  attached but is *not currently inside a Java call* is, from the VM's
  perspective, running native code with no Java frames. Its `root_snapshot` is
  empty and its frames are empty, so the safe state is the same as a
  *running native* thread: the moving collector must not run, OR the thread must
  be counted and "arrive" trivially. We model the **idle-attached** state as a
  blocking region: on return from the outermost Java call we
  `mark_blocked_region_enter` (excluded from `expected`, roots already deposited
  = empty), and on the next Java call we `mark_blocked_region_leave`. This keeps
  STW from waiting forever on a foreign thread that is sitting in the host event
  loop, while still making it rejoin `expected` cleanly when it next runs Java.
  (Equivalently: an attached-but-idle thread behaves like a thread parked in
  `Object.wait`.)
* **Blocking inside a Java call.** Already handled — the existing blocking-site
  protocol (`enter_blocked`/`GcBlockState`/`fold_pointer_map_into_blocked`)
  applies unchanged because the thread is a normal registered thread.

### 3.4 Detach: deregister and reclaim

`jni_detach_current_thread`:

```
fn detach(vm) -> JInt {
    let shared = process_vm()?;
    let jt: Box<JvmThread> = reclaim the boxed thread from TLS;   // take ownership
    let tid = jt.thread_id;
    // If currently inside a blocking/idle region, leave it first so we are not
    // counted/excluded inconsistently, then mark dead.
    shared.thread_registry.mark_dead(tid);       // drops out of alive_count/expected
    // Drop the JvmThread: returns its TLAB to the heap, frees frames/pools.
    drop(jt);
    clear_jni_thread();
    clear_jni_context();
    JNI_OK
}
```

* `mark_dead` (thread_registry.rs L293) flips the registry `alive` flag so the
  next `request_stw` no longer expects this thread. The shared `Arc` fields
  (root_snapshot, gc_block_state) stay in the registry entry until the entry is
  reaped, which is fine: an empty snapshot scans to nothing.
* **Detach must not race a live STW.** If a GC is requested between the last
  Java call and detach, the thread must wait the pause out before tearing down
  its TLAB (the collector may be walking its root snapshot). Reuse the
  `BlockedGuard::drop` discipline: detach enters a checked blocked-leave that
  waits while `stw_requested` is set, then proceeds.
* **TLAB reclamation.** Dropping the `JvmThread` drops its `Tlab`; the unfilled
  tail must be retired/parked the same way a terminating worker does (see the
  TLAB GAP_FILLER retire-on-refill discipline referenced in
  `conservative_roots.rs`). If a thread attaches/detaches repeatedly, each attach
  takes a fresh empty TLAB and each detach retires it.

### 3.5 `Thread.currentThread()` and the Java `Thread` object (later increment)

A fully faithful attach creates a `java.lang.Thread` instance, sets
`thread_status`, names it (`AttachCurrentThreadArgs.name` when provided), groups
it under the system/`main` thread group, and stores it via
`register_with_daemon(..., Some(thread_obj))` + the `thread_obj_to_park` reverse
index so `LockSupport.unpark(thread)` and cross-thread stack dumps work. This is
**deferred** behind the first correctness milestone: the C-level attach that can
safely call a static method is the high-value 80%; constructing the Java
`Thread` object touches class init and is a separable step.

---

## 4. Incremental delivery plan (each step builds green, independently mergeable)

Every step is gated so the default behaviour is unchanged until the whole chain
is validated; the gate flips on at the end (Step 7).

1. **Process-VM cell (scaffold).** Add `set_process_vm` / `process_vm` in
   `jni.rs` and publish from `JNI_CreateJavaVM`. No behaviour change — nothing
   reads it yet. *Green: existing tests; a unit test that
   `process_vm()` returns the VM after create.*
2. **Foreign `JvmThread` factory.** A `fn attach_foreign_thread(&SharedVm,
   daemon) -> *mut JvmThread` helper that boxes a `JvmThread`, allocates a
   `ThreadId`, registers it, mirrors the shared Arc fields, and parks the box in
   TLS. Not yet wired into the JNI table. *Green: a Rust unit test that calls it
   and asserts `alive_count` incremented and the registry has the shared
   snapshot.*
3. **Wire `AttachCurrentThread` (running-call path only).** Make
   `jni_attach_current_thread` call the Step-2 helper via the Step-1 cell and set
   `JNI_THREAD`. Re-attach stays a no-op. *Green: a C/Rust test attaches a thread
   and calls a static `void` method that allocates a little; no GC running yet.*
4. **Detach + teardown.** Implement Step-3.4: reclaim the boxed thread,
   `mark_dead`, drop (retire TLAB), clear TLS, with the wait-out-STW guard.
   *Green: attach/detach in a loop N times from one foreign thread; `alive_count`
   returns to baseline; no leak (TLAB retired).*
5. **Idle-attached blocking region.** On exit from the outermost Java call,
   enter the blocked region; on entry, leave it (§3.3 bullet 2). *Green: a test
   with one foreign thread attached-and-idle while the main thread forces a GC —
   GC completes (does not wait forever) and the foreign thread's later Java call
   still works.*
6. **Concurrent-GC soak.** A multi-threaded embedding test (see §6): K foreign
   threads each attached, looping `Call…Method` that allocates churny garbage,
   while a moving young-gen GC runs. *Green: no UAF/SIGSEGV, output matches
   HotSpot for the same program, under both `--nojit` and JIT.*
7. **`AttachCurrentThreadAsDaemon` + flip default.** Point `t[7]` at a daemon
   variant; remove the temporary opt-out gate so attach is the real path.
   Document in `embedding-api.md`.
8. **(Deferred) Java `Thread` object** (§3.5) — `Thread.currentThread()`,
   naming, daemon flag, group, unpark reverse index.

### Implementation status (landed, Steps 1–7)

All seven steps landed on `feat/foreign-thread-attach`, structured as planned:

- **Step 1** — `jni::PROCESS_VM` (`Weak<SharedVm>`) + `set_process_vm` /
  `process_vm`, published by `JNI_CreateJavaVM` and `cratonvm_create`.
- **Step 2** — `attach_foreign_thread(&SharedVm, daemon, name)` /
  `detach_foreign_thread`, `FOREIGN_THREAD_BOX` + `FOREIGN_CALL_DEPTH` TLS.
- **Steps 3–5** — `attach_current_thread_impl` (running-call path + idle blocked
  region), `jni_detach_current_thread` teardown, and `ForeignCallGuard` (the
  outermost-call idle↔running transition) wrapping the 3 call helpers +
  `NewObjectA`. Gated `CRATONVM_FOREIGN_ATTACH`.
- **Step 6** — opt-in concurrent-GC soak (`--cfg foreign_attach_soak`); green
  JIT-on and `--nojit`.
- **Step 7** — `AttachCurrentThreadAsDaemon` wired to `t[7]`; gate flipped
  **default-on** (opt-out `=0`); docs.

Two findings during implementation, beyond the plan:

1. **Latent `JNIEnv*` indirection bug (fixed).** The historical
   `AttachCurrentThread` stub wrote `JNI_TABLE_PTR.load()` (the table-array
   pointer) into `*penv` — one level too shallow for a `JNIEnv`
   (`*const *const usize`). `(*env)[slot]` then read a function's code bytes as a
   slot pointer and jumped to garbage. Harmless only because nothing ever drove
   the function table through the attach env (the creating thread uses
   `get_jni_env`/the flat API). Fixed: attach now hands back `get_jni_env()`,
   matching `GetEnv`. This was the first crash the soak surfaced.

2. **Idle creating-thread caveat (§5 refinement).** A thread that parks *outside*
   the VM (the creating/coordinator thread in a host `join()` or event loop)
   while foreign threads drive GC is still counted in `request_stw`'s `expected`
   but never reaches a Java safepoint → STW hang. Foreign *attached* threads
   handle this via the idle-blocked model; the **creating** thread has no
   automatic in-native hook yet, so the soak brackets its host-side wait in a
   blocked region (`mark_blocked_region_enter`/`leave`). A clean host-facing
   "this thread is now in native" primitive (or auto-blocking the creating thread
   on return from `JNI_CreateJavaVM`, leaving it on the next VM call) is the
   natural follow-up.

Still deferred (unchanged): the Java `Thread` object (§3.5) so
`Thread.currentThread()` works from attached code; a real `JavaVM*→SharedVm`
back-pointer (§3.1 option b); and `DestroyJavaVM` waiting on attached non-daemon
threads (§5).

---

## 5. Risks & open questions

* **GC race during the attach window.** Between allocating the `ThreadId` and
  publishing `JNI_SHARED_VM`, a STW could be requested. Mitigation: register +
  share `root_snapshot`/`gc_block_state` first (empty snapshot is safe to scan),
  publish TLS last. Needs a focused test that requests STW mid-attach.
* **`JvmThread` address stability vs the JIT.** The JIT bakes
  `tlab_offset`/`shadow_stack_offset`-relative addresses off the live
  `JvmThread*`. The boxed thread must never be moved or reallocated while the
  thread is attached. Boxing (heap, stable address) handles this; a test should
  attach, JIT-compile a hot method on the foreign thread, GC, and verify roots.
* **Idle-attached modelling.** Treating attached-idle as a blocking region is the
  crux. If it is *too* eager (entering blocked on every nested-call return) it
  thrashes `threads_blocked`; scope it to the **outermost** Java frame only.
  Open question: do we instead want an explicit "this thread is a mutator but has
  no Java frames right now → arrives trivially" state, closer to HotSpot's
  `_thread_in_native`? The blocking-region reuse is lower-risk first.
* **Detach from the wrong thread / double attach.** JNI requires detach on the
  same thread that attached and forbids detaching a thread with Java frames on
  its stack. We should return `JNI_ERR` (matching HotSpot) rather than corrupt
  state; needs explicit checks.
* **`DestroyJavaVM` interaction.** Today it is a no-op. A real destroy must wait
  for all non-daemon (including attached non-daemon) threads. Out of scope here
  but the registry already distinguishes daemon vs non-daemon
  (`is_daemon`/`non_daemon` enumeration), so attach must set the daemon flag
  correctly.
* **Thread name / group.** Deferred (§3.5); until then attached threads get a
  synthetic `Thread-N` name and no Java `Thread` object, so
  `Thread.currentThread()` from attached code is the next gap a real app hits.

---

## 6. Validation / acceptance

A change is acceptable when a foreign thread can attach, run Java that allocates
and triggers GC, and detach, with **byte-identical observable behaviour to
HotSpot** and **zero UAF/crash** under a moving collector.

* **Multi-threaded embedding harness (C and Rust).** A small host
  (`scratch/foreign-attach/` or a `libcratonvm` integration test) that
  `JNI_CreateJavaVM`s once, then spawns K host threads. Each thread:
  `AttachCurrentThread`, in a loop call a static method that allocates short-lived
  arrays (forces young-gen churn), occasionally call one that retains a few
  objects, then `DetachCurrentThread`. Run with a small `-Xmx` so GC fires often.
  * **Pass:** process exits 0, no SIGSEGV, no `wait_for_all` hang (watchdog
    clean), per-thread output deterministic.
* **HotSpot differential.** Build the same C host against a real `libjvm` and
  against `libcratonvm`; diff stdout. They must match. (Same methodology as the
  existing functional batteries; run JIT-on and `--nojit`.)
* **Targeted unit/Rust tests** for each increment as listed in §4 (attach
  increments `alive_count`; detach restores it and retires the TLAB; STW
  requested mid-attach completes; GC while a foreign thread is attached-idle does
  not deadlock; a JIT'd hot method on a foreign thread survives a GC).
* **Sanity vs the golden bench.** The single-threaded `bt18 = 68332206`
  bintrees checksum must be unaffected (attach is additive; no foreign thread in
  that run). Cross-check after any change touching `request_stw`/`alive_count`.

---

## 7. Scaffolding to land first (minimal, compiling)

These are the smallest pieces that compile green and let later increments be
small, behaviour-preserving diffs. Described here; implementation is per §4.

* **`jni.rs`: process-VM cell.**
  `static PROCESS_VM: Mutex<Option<Weak<SharedVm>>>` (or `Arc`), plus
  `pub fn set_process_vm(Arc<SharedVm>)` and `pub fn process_vm() ->
  Option<Arc<SharedVm>>`. Published by `JNI_CreateJavaVM` (one extra line in
  `libcratonvm/src/lib.rs` right after `set_jni_context_arc`). Inert until read.
* **`jni.rs`: foreign-attach helper signature.**
  `pub fn attach_foreign_thread(shared: &SharedVm, daemon: bool) -> *mut
  JvmThread` and `pub fn detach_foreign_thread(shared: &SharedVm)` — initially
  the attach helper can be the full Step-2 body; the JNI table keeps calling the
  current stub until Step 3. Keeps the registration logic in one testable place.
* **TLS slot for the owned thread box.** A `thread_local! { static
  FOREIGN_THREAD_BOX: RefCell<Option<Box<JvmThread>>> }` in `jni.rs` so the
  `JvmThread` whose raw pointer lives in `JNI_THREAD` is kept alive and is
  reclaimed on detach. (The main thread keeps its `JvmThread` in `Vm`; foreign
  threads have nowhere else to park it.)
* **Feature gate / env knob.** `CRATONVM_FOREIGN_ATTACH` (default **off** until
  Step 7), read once at attach. While off, `AttachCurrentThread` keeps today's
  env-only behaviour, so every intermediate merge is a no-op for existing
  callers. The flip in Step 7 makes the real path default-on (opt-out =
  `CRATONVM_FOREIGN_ATTACH=0`), matching the project rule that real behaviour is
  the default and the synthetic/legacy path is the opt-out safety net.
* **`AttachCurrentThreadArgs` struct (POD).** A `#[repr(C)]` mirror of
  `JavaVMAttachArgs { version: jint, name: *const c_char, group: jobject }` so
  the `name`/daemon plumbing in §3.5 has a typed landing spot even before it is
  consumed.

None of the above changes runtime behaviour on its own; together they make the
attach/detach implementation a sequence of small, independently-validatable diffs.
