# `class_manager` RwLock same-thread recursive-read deadlock (was: "writer starvation") — FIXED

## Status

**FIXED** (2026-07-06/07, branch `fix/class-manager-writer-starvation-20260706`).
This retires `docs/known-issues/class-manager-rwlock-writer-starvation.md`. The
symptom was real and still reproduced on dev tip `43d1a950` (hang on iteration
1 and 2 of two consecutive 40-iteration hunts), but the documented root cause
("writer starvation under heavy read pressure because parking_lot's non-fair
mode is not writer-preferring") was **wrong on both counts** — the actual bug
was a same-thread recursive read acquisition, and parking_lot's task-fair
policy is precisely what turns that recursion into a permanent deadlock.

## Symptom

`org.springframework.http.client.HttpComponentsClientHttpRequestFactoryTests`
hung intermittently (~25-50% of runs on the Azure Linux host) during
`@AfterEach` teardown (`MockWebServer.close()`), with near-zero CPU burned.
Survived both vtable_manager/class_manager AB-BA fixes (`caa4ee65`,
`60e2b20d`/`48b3c2d2`) — this was a third, independent defect on the same lock.

## Why the original "writer starvation" hypothesis was wrong

Two observations in the ORIGINAL doc already contradicted it:

1. **Near-zero CPU.** Starvation-by-reader-stream means readers keep
   acquiring/releasing — that burns CPU continuously. Zero CPU across the
   window means every party is parked; nobody is streaming through the lock.
2. **Identical gdb captures 2s apart.** Under churn the reader threads would
   be at ever-different PCs. Identical thread sets/PCs = everyone parked.

And the premise itself was wrong: `parking_lot::RwLock` (0.12) implements a
**task-fair** policy — a queued writer blocks NEW readers (they park behind
it in `lock_shared_slow`), so a writer cannot be starved by a stream of new
readers in the first place. What task-fairness DOES make fatal is a thread
that already holds a read guard trying to `.read()` the same lock again:
the second acquisition parks behind the queued writer, while the writer
waits for the first guard to be released — which never happens. This
recursive-read hazard is documented by parking_lot itself (it is why
`read_recursive()` exists).

The doc's "no thread holds the lock in either capture" claim was never
establishable from backtraces at all: a held read guard is just a +16 on the
lock's state word; the holder shows no lock-related frame.

## Actual root cause (proved on a debug-info build, dev tip 43d1a950)

Rebuilt with `debug = true` (line tables + vars; identical codegen) and
caught a live hang. Decisive facts:

- The `class_manager` `RawRwLock` state word read directly from the hung
  process: **`0x1b`** = `WRITER_BIT | WRITER_PARKED_BIT | PARKED_BIT` +
  **reader count = 1**. Exactly one read guard held, writer queued+parked
  (in `wait_for_readers`, `prev_value=0`), other readers parked behind it.
- The holder was one of the "blocked reader" threads ITSELF (`Thread-28`),
  whose inlined frame chain the debug build finally exposed:

```
lock_shared_slow  (PARKED — second, nested read acquisition)
  native_shadow_suppressed_by_redefine   vm/src/runtime/interpreter.rs:20041   <- shared.class_manager.read() AGAIN
  hierarchy-walk closure                 vm/src/runtime/interpreter.rs:20567   <- called while `cm` guard from :20544 is LIVE
  Option::or_else
  try_stackless_invoke                   vm/src/runtime/interpreter.rs:20528
  execute_invokestatic                   vm/src/runtime/interpreter.rs:21218
```

`try_stackless_invoke`'s native-override hierarchy walk (the `.or_else`
closure) acquires `let cm = shared.class_manager.read()` (line 20544) and
then, per ancestor, calls `native_shadow_suppressed_by_redefine(shared, ..)`
(line 20567) — which re-acquired `shared.class_manager.read()` internally
(line 20041). Same thread, two overlapping read guards on one lock.

Deadlock cycle (3 parties, 1 lock):

1. `Thread-28` holds read guard #1 (20544), inside the ancestor loop.
2. `main-vm` calls `load_class_concurrent` -> `class_manager.write()`
   (vm_init.rs:3542) — sets WRITER_BIT, parks in `wait_for_readers`
   waiting for guard #1 to drop.
3. `Thread-28` reaches 20567 -> 20041 -> `.read()` #2 -> task-fair slow path
   parks behind the queued writer. It now waits (transitively) on itself.
4. Every other thread that touches `class_manager` parks too.

### Why it was flaky (~25-50%) and teardown-correlated

`native_shadow_suppressed_by_redefine` fast-paths on the global
`any_class_redefined()` flag: the nested `.read()` at 20041 is only
reachable AFTER a JVMTI agent has redefined some class. This test uses
Mockito (inline mock maker -> class redefinition), so the flag arms partway
through the run. The window then needs a concurrent `class_manager.write()`
(class load) to land between guard #1 and the nested read — teardown
(`MockWebServer.close()`) is exactly when the main thread does a burst of
lazy class-loading (`Stream` pipeline classes for JUnit callback processing)
while pooled HTTP/executor worker threads are still dispatching.

### Why the original session missed it

- Both `.read()`s attribute to `try_stackless_invoke` in a symbols-only
  release binary — `native_shadow_suppressed_by_redefine` is `#[inline]`,
  so the nested acquisition was invisible; the parked thread just looked
  like ONE innocent reader waiting.
- The session audited `execute_invokestatic` / `execute_invokevirtual_vtable_fast`
  for same-FUNCTION reentrancy and correctly found their guards tightly
  scoped — but the recursion was across a helper call inside a closure in a
  DIFFERENT function (`try_stackless_invoke`).

## Fix

`vm/src/runtime/interpreter.rs`: added a guard-reusing variant
`native_shadow_suppressed_by_redefine_in(cm: &ClassManager, class_name)`;
the public wrapper acquires the lock once and delegates; the hierarchy-walk
closure now passes its already-held guard instead of re-acquiring:

```rust
let parent_redefined = native_shadow_suppressed_by_redefine_in(&cm, &parent.name) && ...
```

Audited all 6 call sites of `native_shadow_suppressed_by_redefine`
(interpreter.rs 20149, 20314, 20567, 20594, 20775, 21669): only 20567 sat
inside a live `class_manager` guard scope (20594 runs after the closure's
guard drops; 20775 after an explicit `drop(cm)`; the rest take no guard).
Also audited the co-victim park sites (`populate_virtual_invoke_cache:28229`
takes a fresh guard after `drop(cm)` at 28215) — clean.

## Verification

- Before fix (dev tip 43d1a950): hang on iteration 1 of a 40-iteration hunt
  (release binary) and iteration 2 (debug binary) — 2 hangs within 3 total
  attempts across two builds.
- After fix (same host, same harness, debug binary): 0 hangs in 40
  iterations, all `found=18 succ=18`.
- After fix (final release binary, debug info stripped as on dev): 0 hangs
  in 20 iterations, all `found=18 succ=18`.

## Reproduction (for the record)

```bash
# Azure host; note the /data/data/data mount-depth remap of the shared CP file
BIN=<cratonvm binary>
CP="$(sed -e 's|^/data/data/|/data/data/data/|' -e 's|:/data/data/|:/data/data/data/|g' \
      /data/data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"
for i in $(seq 1 40); do
  timeout 60 $BIN --java-home /data/data/data/jdk25-real \
    -cp "/data/data/data/spring-suite-runner-shared:$CP" \
    KRun org.springframework.http.client.HttpComponentsClientHttpRequestFactoryTests
done
# Pre-fix: ~25-50% of iterations wedge in teardown. To inspect a live hang:
#   sudo gdb -p <pid> -batch -ex 'thread apply all bt'
#   # writer thread frame gives the lock address; then:
#   sudo gdb -p <pid> -batch -ex 'print/x *(unsigned long*)<lock addr>'
#   # state>>4 = held reader count; 0x1b = 1 reader + writer parked = this bug.
```

## Cross-references

- `caa4ee65` / `60e2b20d` — the two (independent, same-shape) AB-BA
  vtable_manager/class_manager fixes this bug hid behind.
- `docs/internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md`
  — the redefine-dispatch work that introduced heavy use of
  `native_shadow_suppressed_by_redefine` on hot dispatch paths.
