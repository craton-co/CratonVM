# Fix note — vm-threading-gc (B1 + B2)

Report: `docs/reviews/fable-2026-06-10/vm-threading.md` (findings B1, B2).
Owned files: `vm/src/threading/gc_barrier.rs`, `vm/src/threading/jvm_thread.rs`,
`vm/src/threading/thread_registry.rs`.

---

## B1 (HIGH, GC safety) — async-exception slot is not a GC root and is never remapped

### Finding
`ThreadEntry.async_exception_slot: Arc<AtomicUsize>` (thread_registry.rs:67)
stores `throwable.as_ptr() as usize`, written by `post_async_exception`
(`Thread.stop`-class cross-thread posting) and read by `take_async_exception`
at the target's next safepoint. The slot was:
- **not scanned** as a GC root — `collect_all_root_snapshots` ignored it; and
- **not remapped** after a moving GC — `update_thread_objs_after_gc` ignored it.

A freshly-allocated `Thread.stop` throwable is generally NOT frame-reachable on
the target, so between post and consume a young/moving GC could **collect** the
throwable (no root kept it alive) or **relocate** it (the raw slot then
dangles). The target would raise a reclaimed/garbage object.

Note: this is a DIFFERENT slot from `JvmThread.pending_async_exception`
(jvm_thread.rs:354), which is already scanned in `roots.rs:133-135` and is the
target's own copy consumed at `interpreter.rs:1407`. The registry slot is the
cross-thread **handoff** location and was the unrooted one.

### Root cause
The slot was treated as a fire-and-forget address with a docstring assumption
("roots from the target's frames will pick it up once stored") that only holds
in the narrow case where the throwable was already frame-reachable.

### Exact change (thread_registry.rs, all within owned file)
1. `collect_all_root_snapshots` now also pushes a non-zero
   `async_exception_slot` as a root (round-tripped via `ObjectRef::from_raw`),
   scanned regardless of `alive` (a dead-but-unreaped thread may still consume
   it on a final safepoint).
2. `update_thread_objs_after_gc` now repoints a non-zero slot through the GC
   pointer map (load → `pointer_map.get` → store), mirroring the existing
   `java_thread_obj` remap in the same loop.
3. Updated the field doc, `post_async_exception` doc, and added the B1 cross
   references.

No wiring outside owned files is required: `collect_all_root_snapshots` is
already called by the root scanner (`roots.rs`, also `vm.rs`,
`interpreter.rs`, `hprof.rs`) and `update_thread_objs_after_gc` is already
called by the GC initiator under STW at **gc.rs step 21** (`memory/gc.rs:329`).
The remap runs under STW, so no mutator can race the slot's load/store (a
blocked thread executes no code and cannot call `post_async_exception` during
STW); this matches the existing un-atomic `java_thread_obj` remap in the same
function.

### Tests added (thread_registry.rs)
- `b1_async_exception_slot_is_a_root` — posting makes the throwable appear in
  `collect_all_root_snapshots`; consuming removes it.
- `b1_async_exception_slot_is_remapped` — after a simulated moving GC
  (`update_thread_objs_after_gc` with `old→new`), `take_async_exception`
  returns the **relocated** address, not the stale one.
- Helper `dummy_aligned_objref` builds a non-null 8-byte-aligned stand-in
  address from a live `[u64; 2]` (the registry never dereferences it — it only
  round-trips the address).

---

## B2 (HIGH, GC safety) — excluded blocked threads could inflate `arrived`

### Finding
`request_stw` deliberately excludes blocked threads from `expected`
(`expected = alive - 1 - blocked`). But `arrive_and_wait` incremented
`inner.arrived` and fired `all_arrived` on `arrived >= expected` for ANY
non-initiator caller while STW was active. An excluded (blocked) thread that
reached `arrive_and_wait` could push `arrived` to `expected` before a counted
mutator arrived, releasing the initiator's `wait_for_all` early — the moving
collector then runs under a live mutator holding raw `ObjectRef`s in Rust
locals. The old in-code comment claimed this was "harmless because
`wait_for_all` uses `<`", but the `>=` notify in `arrive_and_wait` makes the
spurious arrival release the quota.

### Root cause
`arrive_and_wait` had a single code path that counted every caller; it had no
way to express "wait out the pause without counting" for a thread that
`request_stw` excluded. The releasing condition (`>=`) plus an unconditional
`arrived += 1` is the hazard.

### Exact change (gc_barrier.rs, all within owned file)
- Split `arrive_and_wait` into a private `arrive_and_wait_inner(tid,
  participating: bool)`:
  - `participating == true` → increments `arrived` and signals `all_arrived`
    on `>= expected` (the prior, correct behavior for a counted mutator).
  - `participating == false` → only waits out the pause; never touches
    `arrived` or `all_arrived`.
- Kept `pub fn arrive_and_wait(tid)` (signature unchanged → no external call
  site breaks) routing to `participating = true` — this is the genuine
  interpreter-safepoint entry (`interpreter.rs:1389`) and the `pre_stw == true`
  blocked-transition paths, both of which ARE counted in `expected`.
- Added `pub fn arrive_and_wait_excluded(tid)` → `participating = false` for any
  thread that `request_stw` excluded (blocked in a native) and that must wait
  the pause out without counting.
- Refreshed the now-stale "harmless over-count" comment in `request_stw`.

### Why no external call site is changed (and the one to watch)
At every current `arrive_and_wait` call site the caller is participating
**by construction**:
- `interpreter.rs:1389` — genuine safepoint, counted mutator.
- The `if blk.pre_stw { arrive_and_wait }` paths (vm_exec.rs:641/3157/3201/
  3604/4014, vm_util.rs:265, jni.rs:2423) — `enter_blocked`/
  `mark_blocked_region_enter` incremented `threads_blocked` AFTER `request_stw`
  read the count, so this thread WAS counted → participating.
- The wake drain loop `check_post_block_gc_refs` (vm_exec.rs:1011-1016) runs
  AFTER `mark_blocked_region_leave`/`BlockedGuard::drop` has already
  decremented `threads_blocked` and waited the prior STW out; any NEW STW
  therefore counts this thread in `expected` → participating.

So B2 as it stands is a **latent API hazard / defense-in-depth** gap (exactly
report feature-suggestion 3): the API permitted an excluded caller to
over-count and release early, even though current control flow does not exercise
it. The fix removes the hazard and makes the invariant explicit and callable.

**Follow-up for the owner of `vm/src/vm/vm_exec.rs` (NOT my file):** if a future
change ever drains the barrier from a thread that is still inside its blocked
region (i.e. still counted in `threads_blocked`, hence excluded from
`expected`) — e.g. relocating the `check_post_block_gc_refs` drain loop
(vm_exec.rs:1011-1016) to run BEFORE `mark_blocked_region_leave` decrements —
that drain must call `gc_barrier.arrive_and_wait_excluded(tid)` instead of
`arrive_and_wait(tid)`. Today that loop runs after the decrement, so it is
correctly participating and stays on `arrive_and_wait`.

### Tests added (gc_barrier.rs)
- `barrier_excluded_thread_does_not_release_early` — 3 alive, 1 blocked →
  `expected == 1`; an `arrive_and_wait_excluded` caller does NOT drop
  `pending_count` below 1, and `wait_for_all` only completes once the counted
  mutator arrives via `arrive_and_wait`.
- `barrier_participating_entry_counts` — regression guard that the normal
  counted path still releases `wait_for_all`.

---

## Files touched
- `vm/src/threading/gc_barrier.rs` — B2 fix + comment refresh + 2 tests.
- `vm/src/threading/thread_registry.rs` — B1 root-scan + remap + docs + 3 tests.
- `vm/src/threading/jvm_thread.rs` — **no code change**; the registry slot, not
  `JvmThread.pending_async_exception` (already rooted), was the B1 culprit.

## Follow-up & risk
- Risk LOW. B1 only adds roots/remaps an address that was previously unmanaged;
  the remap runs under the existing STW lock alongside `java_thread_obj` so no
  new race. B2 keeps the public `arrive_and_wait` signature and behavior
  identical for all current callers (all participating); the only new surface
  is the additive `arrive_and_wait_excluded` method.
- The B2 follow-up above is the single thing a vm_exec.rs owner should keep in
  mind if the blocked-region drain ordering is ever refactored.
