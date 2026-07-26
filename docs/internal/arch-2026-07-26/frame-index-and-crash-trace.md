# `frame-index-and-crash-trace`

Arch pass, 2026-07-26. Base: `arch/wave1-integration-20260726` @ **`9d0b47785`**
merged with the `vm-exec` closeout branch @ **`256ab66c7`** (merge commit
`ae0c06bb9`). The second merge is load-bearing: `vm/src/vm/vm_util.rs`'s
publication mechanism is only on that branch, and both tasks below depend on
it or on the doc it carries.

Task specs: `vm-exec-closeout.md` §5.1 (CR-VXC-1), §5.3 (CR-VXC-3), §5.2
(CR-CLO-2).

| # | Request | Outcome |
|---|---------|---------|
| 1 | Crash handler must render the *faulting* thread's Java frames | **closed** |
| 2 | JNI-attached threads must publish their frames too | **closed** |
| 3 | `Frame` should carry its method index | **landed on `Frame`; the `CachedBytecodeMethod` half specified, not landed** |

Files written: `vm/src/runtime/crash_handler.rs`, `vm/src/native/jni.rs`,
`vm/src/runtime/frame.rs`, `jit-api/src/lib.rs`, this document.

**No struct literal outside this pass's ownership was edited, and none needed
to be.** That is the constraint item 3 turns on; see §3.

This pass could not build or test (nine concurrent cargo builds OOM the host).
All four sources are `rustfmt --edition 2021 --check`-clean on the hunks this
pass added, verified against scratch copies so the check could not rewrite CRLF
into the worktree. `frame.rs` carries three pre-existing rustfmt diffs in test
code this pass did not touch; the count is 3 at `HEAD` and 3 after, i.e.
unchanged. Line endings re-counted byte-for-byte after every edit and unchanged
throughout (all four files 100% CRLF).

---

## 1. CR-VXC-1 — a worker-thread crash now prints that worker's Java frames

`vm/src/runtime/crash_handler.rs`, `java_stack_lines`.

### The gap

`java_stack_lines` read one process-wide `OnceLock`, `PRIMORDIAL_FRAME_TRACE`,
published from `Vm::new`. A fault on a spawned worker or on a virtual-thread
carrier rendered the *primordial* thread's frames — or the "crashed before the
primordial thread was registered" placeholder — never the faulting thread's.
Nothing in the report said which it was, so the reader could not tell a real
stack from an unrelated one. The two crash classes that most need Java frames
(virtual-thread resume heap corruption, STW-takeover deadlock) both fault on
workers, and the report already carries JDK mode, collector state and the
moving-vs-non-moving verdict — the Java frames were the conspicuous gap.

### What landed

```rust
pub fn java_stack_lines(max_frames: usize) -> Vec<String> {
    if let Some(lines) = crate::vm::faulting_thread_java_stack_lines(max_frames) {
        return lines;
    }
    // …existing PRIMORDIAL_FRAME_TRACE body, unchanged…
```

Plus a rewritten doc comment: the function no longer claims to be the
primordial thread's frames, and it now states outright that it allocates on
both paths and therefore must not be reached from the Unix async-signal-safe
handler.

### The safety bar, verified rather than assumed

The task said to check the sibling's accessor rather than trust it. Read at
`vm/src/vm/vm_util.rs:3665-3702`:

* `CURRENT_THREAD_FRAME_TRACE.try_with(...)` — cannot panic on a thread whose
  TLS is being destroyed.
* `cell.try_borrow().ok()?` — cannot panic on a re-entrant borrow.
* `trace.try_lock()` — the `else` arm *renders a line saying the mutex was
  held* rather than waiting. This is the actual crash-time situation (the
  faulting thread died mid-republication) and is pinned by a test here as well
  as in `vm_util.rs`.
* Returns `None` when nothing is published, which is what makes the call site
  strictly additive.

Both existing callers are the allocating report paths — `CrashReport::render`
via `write_vm_section` (the Rust panic hook) and the Windows vectored exception
handler at `crash_handler.rs:1089`, which already calls `vm_diagnostic_lines`.
A workspace-wide search confirms there is no third caller and in particular no
Unix async-signal-safe one, so the allocation constraint is satisfied by
construction rather than by convention.

### Tests (`crash_handler.rs`, `mod tests`)

Each runs inside its own spawned thread — the publication cell is thread-local
and cargo's harness reuses threads, so publishing on the harness thread would
leak into unrelated tests.

* `a_worker_thread_crash_now_renders_that_workers_java_frames` — the frames,
  the thread name, the tid, and an assertion that the output is **not** labelled
  "primordial".
* `an_unpublished_thread_still_falls_back_to_the_primordial_path` — pins that
  the change is additive, including that an unpublished thread never claims a
  faulting-thread trace.
* `the_full_vm_state_section_carries_the_faulting_threads_frames` — through
  `vm_diagnostic_lines`, which is what both report paths actually call.
* `a_held_frame_trace_mutex_is_reported_rather_than_waited_on` — the crash-time
  case; a handler that blocks here turns a diagnosable crash into a hang.

---

## 2. CR-VXC-3 — JNI-attached threads

`vm/src/native/jni.rs`, `attach_foreign_thread` / `detach_foreign_thread`.

§5.3 asked for one line beside the existing `set_frame_trace`, and warned that
whoever owns the file should confirm the guard's lifetime spans the
*attachment*. It does not: `PublishedFrameTrace` is an RAII guard, so binding it
inside `attach_foreign_thread` would un-publish the moment that function
returned — before the attached thread ran a single bytecode. The `let _crash_frames = …`
spelling in the request would have been a silent no-op here, unlike at the two
`vm_exec.rs` sites where the guard's scope really is the thread's Java lifetime.

So the guard is parked in a new thread-local, `FOREIGN_CRASH_FRAMES`, alongside
the existing `FOREIGN_THREAD_BOX` / `FOREIGN_CALL_DEPTH`, and taken back out in
`detach_foreign_thread`. Two ordering details:

* **Publish after dropping any stale guard, not before.** The guard is
  save-and-restore. Assigning a new guard into an occupied cell would drop the
  old one *after* the new publication, and the old guard's `Drop` restores
  *its* predecessor — blanking the trace that was just installed. The stale
  guard is therefore taken and dropped first, so the assignment writes into an
  empty slot and drops nothing.
* **Un-publish before the `JvmThread` box is dropped.** The guard holds only an
  `Arc` to the frame trace, so no dangling reference is possible either way, but
  a guard left in place would make a later fault on this (by then plain host) OS
  thread render the stack of a Java thread that no longer exists.

Both the take and the store use `try_borrow_mut().ok()`, matching the
never-panic discipline of the mechanism they feed.

---

## 3. CR-CLO-2 — `Frame` carries its method index; `CachedBytecodeMethod` does not

### What the index is for

`stackwalker::capture_frames_no_lines` is the thread-dump depositor. It runs on
the blocking thread at every safepoint/blocking deposit and must not take a
`ClassStore` borrow, so it publishes `StackTraceEntry::method_index: None` and
deferred resolution (`resolve_line_numbers_in_place`) falls back to the
unambiguous-name rule. That rule declines on an overload set — overloads share a
name and have different `LineNumberTable`s — so overloaded frames in a
cross-thread dump report no line at all. A `u32` copied out of the frame needs
no borrow, no lock and no allocation.

### Landed — `vm/src/runtime/frame.rs`

* `Frame::method_index: Option<u32>`, a cold field.
* `Frame::method_index(&self) -> Option<u32>` and
  `Frame::set_method_index(&mut self, Option<u32>)`.
* All five `Frame` struct literals initialise it; there are no `Frame` literals
  outside `frame.rs` (the struct has private fields, so there cannot be).

**Deviation from §5.2, deliberate.** The spec put the field on
`FrameInner::Owned`. It is on `Frame` instead, because `FrameInner` has two
variants and a setter that wrote only to `Owned` would silently no-op on a
`Cached` frame — whose metadata `Arc` is shared across every frame of that
method and must not carry per-push state. One field on `Frame` covers both
variants with one accessor and one reset rule. Nothing is lost: the enum is
sized by its `Owned` variant either way, so hiding the field inside it would not
have saved a byte, and the cached-side "resolve once per method" property is
preserved — when `CachedBytecodeMethod` gains its field,
`Frame::new_pooled_cached` seeds this one from the `Arc` with a plain copy
(the line is already there, commented).

### The two staleness hazards, both closed

`resolve_line_numbers_in_place` guards an index by re-reading
`class.methods[idx]` and re-checking that its **name** matches. That catches a
wrong name. It does **not** catch another member of the same overload set —
which is the only population the index exists to disambiguate. So a stale index
is strictly worse than no index: it can print a line from the wrong method body,
which the fallback rule never does.

* **`reset_for_tail_call`** clears it. Tail-call elimination reuses the frame
  allocation under a different method; an index left behind would be read back
  as the callee's slot. Pinned by
  `a_tail_call_clears_the_method_index_rather_than_stranding_it`, which tail-calls
  into a method with the **same name** on purpose, so the name check cannot save
  it.
* **`from_frozen_frame`** sets it to `None` explicitly rather than by omission.
  `FrozenFrame` carries no slot today, so the value would be `None` anyway — but
  spelling it means that if the frozen shape ever gains an index, this site has
  to make a deliberate decision about re-verifying it on the *resuming* side. A
  continuation can be thawed long after a redefinition reordered the overload
  set it was captured from, and the name re-check cannot catch that reordering.
  Pinned by `a_thawed_continuation_frame_carries_no_method_index`.

### Not landed: the `CachedBytecodeMethod` field — and the repurposing trick does not apply

The task asked whether the trick that saved the `native_callback_cache` change
(repurpose an existing field's *type*, so every literal's `OnceLock::new()`
still infers) applies here. **It does not**, for a reason independent of the
literals:

* **Adding a field is out.** 38 struct literals in eight files across four
  crates (`cratonvm-jit-api`, `cratonvm-jit`, `cratonvm-classloading`,
  `cratonvm-vm`, plus `jit/tests/ir_vs_singlepass.rs`). Only five have a
  `..expr` tail — the `..make_cached_method()` literals inside `jit-api`'s own
  tests — and those are the dangerous ones: they would keep compiling silently
  while the other 33 errored. Editing 33 literals in files this pass does not
  own would have left the workspace non-compiling for eight concurrent agents.
* **Repurposing a field's type is possible but useless.** All 38 literals spell
  the four `OnceLock` fields as `std::sync::OnceLock::new()`, so an
  `OnceLock<T>` → `OnceLock<U>` change really would leave every literal
  compiling. But the blocker is not the declaration — it is **population**. All
  eight *production* construction sites are outside this pass's ownership:
  six in `vm/src/runtime/interpreter.rs`, one in `vm/src/runtime/vtable.rs`, one
  in `vm/src/jit/helpers.rs`. A repurposed cell would be declared here and set
  nowhere, which is exactly the default-off landing rule 3 forbids. On top of
  that, `force_native_cache`'s two production readers (`interpreter.rs:30012`
  and `:39381`) would break on a type change, and `invoc_key`'s `OnceLock`
  is initialised lazily by `invoc_key()`, so a second writer racing it would
  lose non-deterministically.

So the field is **specified in place**, in a long doc block on
`CachedBytecodeMethod` in `jit-api/src/lib.rs` — the file the next owner will
already be reading. It names the exact eight production sites and, for each, what
it has in scope:

* six `interpreter.rs` sites (`try_invoke_cached_lambda_impl`,
  `populate_invoke_cache`, `try_jit_upgrade_with_gate`,
  `try_jit_compile_callee_slow`, `populate_virtual_invoke_cache`) and
  `helpers.rs::try_resume_trapped_callee` hold a live `ClassStore` borrow plus
  the resolved `&ClassFileMethod` and declaring `ClassId` — the index is free
  there;
* `vtable.rs::vtable_install_adapter` has no store but already receives
  `VtableSlotDescriptor::method_index`, resolved at link time, and can pass it
  straight through;
* the first-call tier-up deopt-resume path in `interpreter.rs::execute` has
  neither — it drops its `class_manager` guard before building the entry — so
  `None` is correct there and costs only that one path the fallback rule.

The same block documents the four-crate blast radius of *any* field addition, so
the next person does not re-derive it.

### The remaining consumer line

`stackwalker::capture_frames_no_lines` still spells `method_index: None`. One
line, in a file this pass does not own:

```rust
method_index: f.method_index(),
```

That function's own comment already names this as the change that would make it
free. Until it lands, `Frame::method_index()` is reachable but nothing in the
production capture path reads it.

### Tests (`frame.rs`, `mod tests`)

* `a_frames_method_index_starts_absent_and_round_trips_through_the_setter`
* `a_tail_call_clears_the_method_index_rather_than_stranding_it`
* `a_thawed_continuation_frame_carries_no_method_index`
* `a_cached_frame_reports_no_index_until_the_shared_entry_carries_one` — pins the
  documented state of the cached half, and that the per-frame field is still
  writable for a cached frame (which is what the uniform placement buys).
* `an_overload_set_resolves_exactly_only_when_the_frame_carries_its_index` — the
  payoff, end to end against a real `ClassStore` built from
  `stackwalker::test_support`: two overloads of `m` with different
  `LineNumberTable`s; without an index the resolver returns 0 and leaves
  `UNKNOWN`, with the index it returns 1 and picks the **second** overload —
  the one a name-only rule could never reach — and the first overload resolves
  to its own line, not the other's. This is written against the exact entry
  shape `capture_frames_no_lines` will produce, so the one-line change above is
  pinned before it is made.

---

## 4. Cross-owner requests raised by this pass

**None of these edits were made.**

### 4.1 `vm/src/runtime/stackwalker.rs`, `capture_frames_no_lines`

Replace `method_index: None` with `method_index: f.method_index()`. Strictly
additive — `Frame::method_index()` returns `None` for every frame today, so the
change is a no-op until a pusher starts setting it, and correct the moment one
does. Pinned by
`an_overload_set_resolves_exactly_only_when_the_frame_carries_its_index` in
`frame.rs`.

### 4.2 `jit-api/src/lib.rs` + the eight production `CachedBytecodeMethod` sites

The field addition, spelled out in §3 above and in full on the struct's doc
comment. It needs `cratonvm-jit-api`, `cratonvm-jit`, `cratonvm-classloading`
and `cratonvm-vm` in one landing, plus `jit/tests/ir_vs_singlepass.rs`. Whoever
takes it should check the five `..make_cached_method()` literals in `jit-api`'s
own tests by hand — they will not error.

### 4.3 `vm/src/runtime/interpreter.rs` — call `Frame::set_method_index`

For `Owned` frames the interpreter is the only thing that knows the slot. The
six cached-invoke sites listed above resolve the method already; the same value
can go into the pushed frame directly via `set_method_index` even before the
`CachedBytecodeMethod` field lands, which would make the index available on the
`Owned` half immediately.

---

## 5. Not done

* **The `CachedBytecodeMethod` field.** Specified, not landed (§3). Landing the
  declaration without the eight population sites would be a default-off landing.
* **The `Throwable` capture path.** Untouched, deliberately. It resolves eagerly
  by exact `(name, descriptor)` and is correct today, including for overloads.
  The index does not make deferring it safe: verification is by *name*, so a
  redefinition that reorders an overload set across the capture/read window is
  the one case verification cannot catch, and eager capture has no such window.
  See `cross-owner-closeout.md` §6.
* **`java_stack_lines`'s primordial-path header text.** The fallback body still
  says "Java frames (primordial thread…", which is now accurate rather than
  misleading — that path *is* the primordial one. Left alone.
* **No environment-variable gate was added and nothing landed default-off.**
