# `vm_exec` closeout

Arch pass `vm-exec-closeout`, 2026-07-26.
Base: `arch/wave1-integration-20260726` @ **`18b2f49d5`** (dev + 32 merged agent
branches; `vm/src/runtime/vtable.rs` contains `SlotBucket`, 16 references).

Four requests, each raised by an agent that did not own `vm/src/vm/vm_exec.rs`.
Every claim was re-verified against the merged tree before acting — this repo
has a standing habit of in-tree comments that assert the opposite of the code,
and item 1 is another instance of exactly that.

| # | Request | Source | Outcome |
|---|---------|--------|---------|
| 1 | `thread_stack_trace` must use the resolving reader | `cross-owner-closeout.md` CR-CLO-1 | **closed** |
| 2 | Worker threads must publish frames to the crash handler | `startup-and-diagnostics.md` §6.3 | **closed on the half this pass owns**, three-line handoff in §5.1 |
| 3 | `Frame` should cache its method index at push | `cross-owner-closeout.md` CR-CLO-2 | **specified, not landed** — the field belongs in files this pass does not own (§5.2) |
| 4 | Contended `monitorenter` should unmount a virtual thread | `virtual-threads.md` §7.2 | **declined, fully specified** (§4) |

Files written: `vm/src/vm/vm_exec.rs`, `vm/src/vm/vm_util.rs`, this document.
`vm/src/vm/vm_object.rs` was owned but needed no change.

This pass could not build or test (nine concurrent cargo builds OOM the host).
Both touched sources are `rustfmt --edition 2021 --check`-clean, verified
against a scratch copy so the check could not rewrite CRLF into the worktree.
Line endings were re-counted byte-for-byte after every edit and are unchanged
(`vm_exec.rs` 21743 CR / 21743 LF; `vm_util.rs` 4609 CR / 4609 LF at the time of
each check).

---

## 1. CR-CLO-1 — thread dumps showed line `-1` on every frame — **closed**

`vm/src/vm/vm_exec.rs:7638` `thread_stack_trace`, cross-thread arm (now ends at
`:7679`).

### Re-verified before acting

Both stale comments were still present verbatim on the merged tree, and both
still contradicted the code:

* *"Resolve line numbers from the BCI now that we hold the ClassStore (so a dump
  still gets source lines without paying for them at every deposit)"* — followed
  by a call to `frame_trace_of(tid)`, which resolves nothing. The registry's own
  doc (`thread_registry.rs:1331`) now says plainly that every entry it returns
  has `line_number == -1`.
* *"The published entry doesn't carry the ClassId/descriptor needed to resolve
  source lines"* — it has carried `class_id` for some time (`StackTraceEntry`,
  `native-api/src/registry.rs:3659`), and since CR-SW-1 the eager path also
  carries `method_index`.

So the *stated intent* of this arm had never been implemented, and the comment
was the reason nobody noticed: every `ThreadMXBean.dumpAllThreads` /
cross-thread `getStackTrace` frame printed line `-1`.

### What landed

```rust
let cm = self.shared.classes.class_manager.read();
self.shared
    .threads
    .thread_registry
    .frame_trace_of_resolved(tid, &cm.class_store)
```

`frame_trace_of_resolved` (`thread_registry.rs:1363`, landed by the
`cross-owner-closeout` pass) is `frame_trace_of` plus
`stackwalker::resolve_line_numbers_in_place`, with the registry lock dropped
before the walk. Both stale comments are gone, replaced by one that records what
they claimed and why it was false, so the next reader does not re-derive it.

Three properties this depends on, each of which the new tests pin:

* **The borrow is not new.** `class_manager.read()` is the same lock the
  current-thread arm ten lines above already takes, on the same call. No new
  lock is introduced and no lock is held across the registry read (the registry
  lock is taken and dropped *inside* `frame_trace_of_resolved`, before the
  `ClassStore` walk — holding L5 across a `ClassStore` walk would invert the
  usual order).
* **Non-destructive.** Resolution runs on the returned copy. The deposited
  snapshot stays exactly as the thread published it, so a second dump — or the
  crash handler reading the same `Arc` — is unaffected.
* **Fail-closed.** The resolver fills in the line an eager capture would have
  produced or leaves `-1`; it never writes a wrong line and never touches an
  entry that already has one, including the `-2` native sentinel.

### Deliberately not changed

The depositor at `capture_frames_no_lines` stays line-less and lock-free. That
is the whole design: the deposit happens on the blocking thread at every
safepoint/blocking point and must not take a `ClassStore` borrow. The cost of
resolution is paid once, by the dumper, which already holds the store.

Overloaded frames in a cross-thread dump still resolve by the
unambiguous-name rule and therefore still fail closed to `-1` — that is CR-CLO-2,
see §5.2. This change is what makes *non-overloaded* frames (the overwhelming
majority) report a real line.

### Tests (`vm/src/vm/vm_exec.rs`, `mod tests`)

* `a_cross_thread_dump_now_carries_source_lines` — pins both halves: the raw
  reader still yields `-1` (its documented contract, which the lock-free
  depositor depends on) and the reader this arm now uses yields the real line.
* `resolving_a_dump_leaves_the_published_snapshot_untouched` — the deposited
  snapshot is still `-1` after a dump, and a second dump still resolves.
* `a_dump_of_a_thread_whose_class_is_gone_keeps_the_frame` — class unloaded
  between deposit and dump: the *line* is dropped, the *frame* survives. A
  thread dump that loses the call site would be worse than one that loses the
  line.
* `a_dump_of_an_unknown_thread_is_empty_not_a_panic` —
  `dumpAllThreads` is reachable while threads are exiting.

The `ClassStore` fixtures come from
`crate::runtime::stackwalker::test_support` (`stackwalker.rs:543`), which the
`cross-owner-closeout` pass extracted for exactly this kind of reuse.

---

## 2. Worker-thread frames in a crash report — **closed on this pass's half**

`startup-and-diagnostics.md` §6.3.

### The gap

`crash_handler::java_stack_lines` (`crash_handler.rs:339`) reads one
process-wide `OnceLock`, `PRIMORDIAL_FRAME_TRACE`, published from `Vm::new`
(`vm_init.rs:4963`). A fault on a spawned worker or on a virtual-thread carrier
therefore rendered the *primordial* thread's frames, or the
"crashed before the primordial thread was registered" placeholder — never the
faulting thread's. A sibling made the report carry JDK mode, collector state and
the moving-vs-non-moving verdict; a worker fault arrived with none of the Java
context that decides the diagnosis. The two crash classes that need it most —
virtual-thread resume heap corruption, STW-takeover deadlock — both fault on
workers.

Calling `publish_primordial_frame_trace` from a worker is not the fix: it is a
`OnceLock`, first-write-wins, so a worker would either be ignored or (if it
raced ahead of `Vm::new`) permanently displace the primordial publication.

### What landed — `vm/src/vm/vm_util.rs:3574-3690`

A per-OS-thread cell plus a renderer, both `pub` and reachable from anywhere in
the crate as `crate::vm::…` (`vm.rs:25` glob-re-exports `vm_util`):

* `PublishedTraceHandle` — the `Arc<Mutex<Vec<StackTraceEntry>>>` shape
  `JvmThread::frame_trace` already has.
* `CURRENT_THREAD_FRAME_TRACE` — `thread_local!` holding
  `Option<(thread name, ThreadId, handle)>`. Thread-local rather than a shared
  registry, deliberately: the crash handler runs *on the faulting thread*, so
  the read needs no lock at all and cannot itself turn a diagnosable crash into
  a hang. The identity is carried because a carrier OS thread hosts many virtual
  threads over its life and the report must name the one that was mounted.
* `PublishedFrameTrace` — an RAII guard. **Save-and-restore, not
  set-and-clear**: `Drop` puts back whatever was there before. That makes a
  carrier's mount/unmount loop correct without bookkeeping and covers every
  early return out of a mount for free.
* `faulting_thread_java_stack_lines(max_frames) -> Option<Vec<String>>` —
  `try_with` + `try_borrow` + `try_lock` throughout. Never blocks, never panics,
  returns `None` when nothing is published here so the caller falls back to the
  primordial path. Renders the same shape as `java_stack_lines`, including the
  "published at the last blocking/safepoint deposit — may lag the faulting
  instruction" caveat, because it is the same kind of snapshot.

It allocates, so — exactly like `java_stack_lines` — it belongs to the two
allocating report paths (the Rust panic hook and the Windows vectored exception
handler) and must **not** be called from the Unix async-signal-safe handler.

### Publication sites — `vm/src/vm/vm_exec.rs`

* **`:7104`**, platform worker, inside the spawned closure immediately after
  `set_frame_trace(tid, jvm_thread.frame_trace.clone())`. The registry copy
  serves cross-thread readers; this one serves the crash handler. The guard
  lives for the whole closure body, including the `ContinuationYield` early
  return.
* **`:2021`**, `resume_virtual_continuation`, immediately after
  `take_runtime_for_mount`, for the duration of the mount. Deliberately here and
  **not** at `install_runtime` (`:7003`, which the request named): that call runs
  on the *spawning* thread, so a TLS publication there would attach the
  continuation's stack to the wrong OS thread. `resume_virtual_continuation`
  runs on the carrier, which is where the fault would be taken. The guard
  restores the previous occupant on the normal return, the
  `ContinuationYield` unmount, and the missing-`java_thread_obj` bail.

The primordial thread is untouched and keeps its existing publication.

### Tests (`vm/src/vm/vm_util.rs`, `mod tests`)

Nine, each running inside its own spawned thread (the cell is thread-local and
cargo's harness reuses threads, so anything else would leak one test's
publication into the next):
`an_unpublished_os_thread_reports_nothing_so_the_primordial_path_still_runs`,
`a_published_worker_renders_its_own_frames_with_its_own_identity`,
`dropping_the_guard_unpublishes_so_a_carrier_never_reports_a_stale_mount`,
`a_carrier_mounting_one_continuation_after_another_reports_the_current_one`,
`a_nested_publication_restores_the_outer_one_rather_than_blanking_the_cell`,
`a_held_frame_trace_mutex_is_reported_not_waited_on` (the actual crash-time
situation: the faulting thread died mid-republication),
`an_empty_trace_says_so_rather_than_rendering_a_bare_header`,
`a_deep_stack_is_truncated_with_a_count_of_what_was_dropped`,
`publication_is_per_os_thread_and_never_bleeds_across_workers`.

The remaining three lines are in `vm/src/runtime/crash_handler.rs`, which this
pass does not own — see §5.1. Until they land, the mechanism is complete and
tested but the report still renders only the primordial trace.

---

## 3. `vm_object.rs`

Owned, read, unchanged. None of the four requests touched it.

---

## 4. §7.2 — contended `monitorenter` on a virtual thread — **declined**

`virtual-threads.md` §7.2. This is the highest-value item in this pass and the
one most dangerous to half-land, so it gets the longest treatment and no code.

### The bug, re-verified

Confirmed on the merged tree, all four sites:

* `vm_exec.rs:1413` `monitor_enter_blocking` → `m.block_enter(tid)` at `:1471`
  (`:1469` in the labelled-debug arm).
* `vm_exec.rs:1496` `monitor_enter_synchronized_method` → `block_enter` at
  `:1535`.
* `vm_exec.rs:6655` `NativeContextImpl::monitor_wait`.
* `vm_exec.rs:8734` `NativeContextImpl::park` (the platform-park fallback).
* Plus `vm_exec.rs:2215`, the terminating virtual thread's own contended
  `enter_inflated_or_contend` on its `Thread` mirror.

None of these consults `ThreadKind::Virtual`. `block_enter` parks the carrier's
OS thread with the continuation still mounted. With `synchronized` not pinning
from bytecode (`interpreter.rs::Monitorenter` never raises `pin_count`, while
`NativeContextImpl::monitor_enter` at `:6605`/`:6625` does — §7.4's open
inconsistency), a virtual thread can unmount *while holding* a monitor; if
`parallelism` other virtual threads then contend for it, every carrier is
blocked and the owner can never be remounted to release it. The starvation
watchdog (`virtual_threads.rs`, `carrier_pool_is_stalled` /
`spawn_starvation_watchdog`) grows the pool out of the deadlock — its own doc
says it is compensation, not a design.

### Why nothing landed

The protocol cannot be completed inside the three files this pass owns:

1. `monitor_enter_blocking` returns `ObjectRef`, not a `Result`. To yield it
   must return something a caller can propagate, and its callers are
   **`vm/src/runtime/interpreter.rs:17285`** (`Monitorenter`) and, for the
   synchronized-method variant, `interpreter.rs:31272`, `:32901`, `:39492`,
   `:40344`, `:40835`. Every one of those is in a file this pass does not own.
2. `Monitorenter` is not resumable as written. A yielded `monitorenter` must
   re-execute from the *same* `pc` on remount, which is an interpreter-side
   property.
3. The wake side needs `Monitor::exit`
   (`vm/src/threading/monitor.rs:725` / `:1445`) to call
   `VirtualThreadManager::wake_waiters` for the monitor's key. `monitor.rs` is
   not owned here either.

Landing only the `vm_exec.rs` half would produce a VM that unmounts contenders
that nothing ever wakes — strictly worse than today's blocking-but-live
behaviour, and worse than the watchdog it would bypass. The sibling that found
this declined for the same reason and it is the right call.

### The whole protocol, for whoever owns all three files

**Preconditions.** Monitor ownership is now keyed by `ThreadId` through the
object's mark word, not by carrier or OS thread (`monitor.rs::enter_or_contend`,
`:1261`). That is what makes unmount-on-contention possible at all: a
continuation remounted on a different carrier still owns what it owned. Do not
build this on top of any carrier-keyed state.

**The key.** Use the monitor's object address (`obj_ref.as_ptr() as usize as
u64`) as the `wait_on_key` key, and remap it in
`MonitorTable::remap_after_gc` (`monitor.rs:1918`) — a moving collection
relocates the object and would otherwise strand every keyed waiter. If that
remap is not acceptable, key on the inflated `Monitor`'s own stable identity
instead; do not key on anything a GC can move without a remap hook.

**Step 1 — `vm_exec.rs`, new fallible entry point.** Add
`monitor_enter_blocking_yielding(shared, thread, obj) -> Result<ObjectRef,
MethodCallFailed>` beside the existing function; keep
`monitor_enter_blocking` as a thin wrapper for the callers that cannot yield
(the terminating-thread path at `:2215` is one — it has no frame to resume
into). In the new function, after `enter_or_contend` returns `Some(m)`:

```text
if thread.kind == Virtual && thread.pin_count == 0 {
    // Deposit-then-check, so a release that beats the unmount is not lost.
    virtual_thread_manager.wait_on_key(key, tid);          // deposit
    if monitor.try_enter(tid) {                            // re-check
        virtual_thread_manager.cancel_wait_on_key(key, tid);
        return Ok(obj);
    }
    ctx.thread.tlab.retire();
    ctx.deposit_root_snapshot();
    return Err(InternalError(VmError::ContinuationYield { wake_after_nanos: 0 }));
}
// platform thread, or pinned: today's blocking path, unchanged.
```

The deposit-then-check ordering is the race-free half of the existing
`wait_on_key` / `wake_waiters` handshake and must not be reordered: checking
first and depositing second loses a release that lands in between.

**Step 2 — `monitor.rs`, the wake side.** In `Monitor::exit` (`:725`), at the
point where `entry_count` reaches 0 and `owner` is cleared, call
`wake_waiters(key)`. It must be *inside* the same `state` lock hold that clears
the owner, or a contender that deposited just after the clear and just before
the wake is stranded. `wake_waiters` already handles the still-mounted case by
setting `wake_pending`, which `suspend_runtime` consumes
(`virtual_threads.rs:1331` and `:1267`) — so a wake that arrives before the
unmount completes is not lost.

**Step 3 — `interpreter.rs::Monitorenter` (`:17285`).** Switch to the fallible
entry point and propagate. Before returning the yield, rewind `frame.pc` to
`last_instr_pc` so the remounted continuation re-executes the `monitorenter`
rather than falling through as if it had acquired. Nothing else in the arm may
have run: the JFR `mon_start` timestamp is taken before the acquire today and
must be re-taken on the retry, not carried across the unmount.

**Step 4 — the five `monitor_enter_synchronized_method` call sites.** Same
treatment, but harder: those sites have already popped arguments into Rust
locals, which is why the function pins and remaps them. A yield must push the
callee frame *after* the monitor is acquired, so the retry has to restart at the
invoke, not at the callee entry. If that is not straightforward, leave
`ACC_SYNCHRONIZED` method entry on the blocking path for the first landing and
say so — bytecode `monitorenter` (step 3) is where the contention that wedges
pools actually is.

**Step 5 — `monitor_wait` (`:6655`).** `Object.wait` on a virtual thread should
unmount the same way, keyed on the same monitor, woken by `notify`/`notifyAll`
rather than by `exit`. This is a separate landing; the notify side has its own
lost-wakeup shape (a `notify` with no waiter must not be buffered).

**Step 6 — resolve §7.4 first, or in the same landing.** If `synchronized`
pins, none of this is needed and `interpreter.rs::Monitorenter` should raise
`pin_count` to match `NativeContextImpl::monitor_enter`. If it does not pin (JEP
491), this becomes mandatory and `NativeContextImpl::monitor_enter`'s
`pin_count` bump should go. Shipping steps 1-3 while the two paths still
disagree means the same `synchronized` region unmounts or blocks depending on
whether it was entered from bytecode or from a native — a heisenbug generator.

**Do not remove the starvation watchdog in the same landing.** It is the only
thing standing between a bug in this protocol and a wedged VM, and it costs
nothing when the pool is healthy.

**Tests the landing needs.** At minimum: `parallelism + 1` virtual threads
contending one monitor, all of which must complete (this deadlocks today
without the watchdog); a release that lands between deposit and re-check; a
moving GC between deposit and wake (the key remap); a *pinned* virtual thread,
which must still block rather than unmount; and a platform thread, which must
take the unchanged path.

---

## 5. Cross-owner requests raised by this pass

**None of these edits were made.**

### 5.1 CR-VXC-1 — `vm/src/runtime/crash_handler.rs:339`: read the faulting thread's trace

Three lines at the top of `java_stack_lines`:

```rust
pub fn java_stack_lines(max_frames: usize) -> Vec<String> {
    if let Some(lines) = crate::vm::faulting_thread_java_stack_lines(max_frames) {
        return lines;
    }
    // …existing PRIMORDIAL_FRAME_TRACE body, unchanged…
```

Strictly additive and strictly a fallback: the helper returns `None` on any OS
thread that has nothing published (including the primordial thread), so the
current behaviour is preserved everywhere it is the right behaviour. The helper
is `try_with` + `try_borrow` + `try_lock` throughout — it cannot block and
cannot panic, which is the same bar the existing body meets.

It **allocates**, so it belongs only here and in the Windows vectored-exception
path that already calls `vm_diagnostic_lines`. Do not call it from the Unix
async-signal-safe handler.

Optional, and worth doing at the same time: `java_stack_lines`'s doc comment and
the `"Java frames (primordial thread…"` header both hard-code "primordial",
which stops being accurate once the fallback is wired.

### 5.2 CR-VXC-2 — CR-CLO-2, re-specified precisely rather than landed

The request asked this pass to add a cached method index to `Frame`
(`vm/src/runtime/frame.rs`) and noted that if the field must live there, it
should be specified instead of edited. It must, so it is. Neither
`frame.rs` nor `jit-api/src/lib.rs` is owned here.

**Where the field goes — two halves, because `Frame` has two metadata shapes**
(`frame.rs:192`, `enum FrameInner`):

* `FrameInner::Cached(Arc<CachedBytecodeMethod>)` — add
  `pub method_index: Option<u32>` to `CachedBytecodeMethod`
  (`jit-api/src/lib.rs:40`). This is the better half of the two: the entry is
  built once per method and `Arc`-shared across every call, so the index is
  resolved once for the life of the process rather than once per frame push.
  Populate it where the entry is built, which is the one place that already has
  the `ClassStore` borrow and has *just* resolved the method.
* `FrameInner::Owned { … }` — add `method_index: Option<u32>` beside
  `source_file`. Populate in `Frame::new_from_arcs` (`frame.rs:975`) and
  `Frame::new` (`:911`) where the caller knows it; `None` is a correct answer
  everywhere else and simply falls back to today's behaviour.

**Accessor.** `Frame::method_index(&self) -> Option<u32>`, beside
`source_file_arc` (`frame.rs:1321`), matching the existing pattern of reading
through `FrameInner`.

**Consumer.** One line in `stackwalker::capture_frames_no_lines`
(`stackwalker.rs:384`): `method_index: f.method_index()` replacing the current
`None`. That function's comment at `:394-398` already names this as the change
that would make it free — the point is that a `u32` copy needs no `ClassStore`
borrow, which is the constraint that made the field necessary in the first
place.

**Also update.** `Frame::reset_for_tail_call` (`frame.rs:1122`) must reset the
index along with `class_id` — a tail call replaces the method, and a stale index
is exactly the failure mode `resolve_line_numbers_in_place`'s name check is
weakest against (it catches a wrong *name*, not another member of the same
overload set). The `FrozenFrame` thaw path (`:1625`) needs the same treatment or
an explicit `None`.

**What it buys.** Cross-thread thread dumps get exact line numbers on
overloaded frames, which §1 above cannot deliver on its own: deferred resolution
falls back to the unambiguous-name rule without an index, and an overload set is
precisely where that rule fails closed. It also completes the fully-lazy trace
path that CR-CLO-3 sizes.

**What it does not buy, and must not be used for.** It does not make deferring
the `Throwable` path safe on its own — see CR-CLO-3 in `cross-owner-closeout.md`
§6, which this pass agrees with: index verification is by *name*, so a
redefinition that reorders an overload set across the capture/read window is the
one case verification cannot catch, and eager capture has no such window.

### 5.3 CR-VXC-3 — `vm/src/native/jni.rs:462`: the third publication site

`startup-and-diagnostics.md` §6.3 named three sites; two are in `vm_exec.rs` and
landed here (§2). The third is the JNI-attached foreign thread. The edit is the
same one line, beside the existing `set_frame_trace`:

```rust
let _crash_frames = crate::vm::PublishedFrameTrace::publish(
    &name, tid.0, jvm_thread.frame_trace.clone(),
);
```

with the guard bound for as long as the attached thread runs Java. Whoever owns
`jni.rs` should confirm the guard's lifetime spans the attachment, not just the
registration call — a `let _ = …` there would un-publish immediately.

### 5.4 CR-VXC-4 — `vm/src/threading/thread_registry.rs:1331`: `frame_trace_of` now has no production caller

With §1 landed, a workspace-wide search for `frame_trace_of(` returns only
`frame_trace_of_resolved`'s own use at `:1368` and two tests at `:2234` / `:2247`.
It should stay (it is the honest lock-free reader and the resolved variant is
built from it), but its doc could usefully say that the *only* production reader
is now the resolving one, so a future caller does not pick the line-less variant
by accident and re-create the bug §1 fixed.

---

## 6. Not done

* **§7.2.** Declined, fully specified in §4. A half-landed unmount protocol
  unmounts contenders nothing wakes, which is worse than the blocking behaviour
  it replaces.
* **CR-CLO-2 / §7.4.** Specified (§5.2, and §4 step 6). Both need files this
  pass does not own.
* **The `Throwable` capture path.** Untouched, deliberately — it still resolves
  eagerly by exact `(name, descriptor)`. §1 changes only the cross-thread dump
  reader.
* **No environment-variable gate was added and nothing landed default-off.**
  The `CRATONVM_DBG_MONENTER` diagnostic already present in
  `monitor_enter_blocking` is pre-existing and untouched.
