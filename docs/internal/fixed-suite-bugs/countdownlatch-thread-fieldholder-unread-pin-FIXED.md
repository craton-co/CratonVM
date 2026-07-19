# CountDownLatch livelock under thread churn + GC pressure: `Thread$FieldHolder` receiver never re-read before nested constructor invoke — FIXED

Status: RESOLVED — fixed 2026-07-17
Severity: was High (100% reproducible hang, ~1s-per-round harness, under `CRATONVM_DBG_GC_STRESS` and, by the same mechanism, plausibly any allocation-heavy real workload's FIRST `new Thread(Runnable, String)`)
Fixed: 2026-07-17, branch `fix/gcbarrier-cdl-wait-livelock-20260717`, merged to `dev` as `0e1a1fa2`

## Symptom

`CdlSpawnRepro.java` — 4 threads each doing 50,000 trivial `new Object()`
allocations then `CountDownLatch.countDown()` (no `Thread.join()` anywhere),
main calls `done.await(30, SECONDS)` — hangs reliably on round 0 of 1 under
`CRATONVM_DBG_GC_STRESS=65536`:

```
CRATONVM_DBG_GC_STRESS=65536 <cratonvm-cli> --java-home <jdk25> \
  -cp <dir> CdlSpawnRepro 4 10
...
@@ROUND r=0 ms=30013 finished=false count=1
@@HANG_DETECTED at round 0
```

`done.getCount()` sticks at exactly 1 — i.e. exactly one of the four worker
threads' `countDown()` calls is never invoked. This is a distinct,
independently-discovered bug from the GC-barrier `expected`/`arrived`
census-exclusion livelock fixed in `cce6e1c6` (a missing
`deposit_root_snapshot()` before `GcBarrier::enter_blocked()` on the
thread-termination "notify waiting joiners" path) — confirmed NOT fixed by
that commit, and confirmed to not even exercise that code path, since this
repro never calls `Thread.join()`.

## Investigation summary

An initial live-gdb capture (from the sibling session that found this bug
while investigating an unrelated Hibernate BigInteger AIOOBE, see
`docs/internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md`) showed only 3
of the expected 5 OS threads (main + 4 workers) via `ps -T`, raising the
question of whether all 4 worker threads even spawn. Fine-grained polling
(`ps -T` every 50ms from process start) resolved this: all 4 spawn
correctly, but under `CRATONVM_DBG_GC_STRESS` their 50,000-allocation loops
finish in ~1-2s, fast enough for a coarse poll to miss one between samples.
Not a thread-spawn bug — a polling-granularity artifact.

A `CRATONVM_DISABLE_JIT=1` run reproduced identically, ruling out a JIT
miscompile. Temporary instrumentation inside `native_cdl_count_down` showed
only 3 of 4 expected `countDown()` calls ever happen — the 4th thread's
`Runnable.run()` never reaches its `countDown()` statement at all, despite
the thread's `Thread.run()` dispatch returning `Ok` (no uncaught exception).
An explicit `System.out.println` marker at the very first statement of the
lambda body confirmed it: that println never printed. The thread's
`Runnable` body never executes a single bytecode instruction.

`java/lang/Thread.run()`'s native override falls back through
`Thread.target` then `Thread.holder.task` by field name. Direct inspection
of the affected Thread's `holder.task` field read back as `Int(0)` — a raw
zeroed slot, not even a stale `Object` reference — while `holder` itself
was confirmed live and correctly forwarded (`load_and_forward(holder) ==
holder`, `num_slots=7` matching real JDK 25's `Thread$FieldHolder` layout
exactly). A genuinely live, correctly-sized object whose `task` field was
simply never written.

## Root cause

Real-JDK-mode `new Thread(Runnable, String)` is a native override
(`native-builtins/src/lib.rs`'s `java/lang/Thread`/`<init>` registration,
delegating to a local helper `populate_real_thread_holder`), not real
bytecode. `java.lang.Thread$FieldHolder`'s own `<init>` is *also* natively
overridden (same file, registered for
`"java/lang/Thread$FieldHolder", "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V"`) —
so the `putfield task` bytecode `javap` shows in the real class body never
actually runs; the native closure's own `set_field_by_name`/`set_field`
calls are the real write path.

In `populate_real_thread_holder`:

```rust
let holder = ctx.alloc_object(holder_class, nfields);   // correctly 7 slots
let holder_handle = ctx.pin_native_root(holder);        // pinned...
let group = match group {                               // ...but this block
    Value::Object(Some(_)) => group,                     // (real allocation-
    _ => {                                                // adjacent work,
        let cur = ctx.current_thread_object();            // run whenever the
        let g = ctx.get_field_by_name(cur, "holder");      // caller passed a
        match g { ... }                                    // null ThreadGroup,
    }                                                       // i.e. every plain
};                                                          // `new Thread(r, n)`)
let args = [
    Value::Object(Some(holder)),   // <-- STILL the pre-pin-refresh local!
    group, target, ...
];
let ctor_ok = ctx.invoke("java/lang/Thread$FieldHolder", "<init>", ..., &args).is_ok();
// Re-read `holder` through the pin only happens HERE, after the invoke:
let holder = ctx.read_native_pin(holder_handle, holder);
```

`holder` is pinned immediately after allocation, correctly anticipating
that a moving GC could relocate it before the nested `FieldHolder.<init>`
invoke runs. But nothing ever read back through that pin *before* `holder`
was used to build the invoke's `args` array — the only re-read happens
*after* the invoke returns, protecting against a GC *during* the invoke
while leaving the window *before* it (the group-resolution block, which
does real heap-touching work) completely unguarded.

`holder`'s from-space copy — the address the collector just evacuated —
remains a *structurally valid* read for as long as that memory goes
unreused: identical header bytes, `num_slots`, `class_id` (movers copy the
whole object verbatim). This is why `CRATONVM_DBG_STRAYSTACK`-style
bounds/shape sanity checks never fired, and why the observed symptom is a
correctly-sized object with genuinely zeroed fields rather than a crash or
an obviously wild pointer: `FieldHolder.<init>`'s `set_field_by_name`
writes land cleanly on the dead from-space copy (a valid write to
already-reclaimed memory — no fault), while every subsequent read of
`Thread.holder.task` correctly reads the *live* copy's untouched,
still-zero `task` slot.

**Why only the FIRST-ever `new Thread(Runnable, String)` in a process is
affected**: this call site's `ensure_class_initialized("java/lang/Thread$FieldHolder")`
(a few lines above the allocation) loads and links that class for the
first time on a fresh VM — real, allocating work, making a GC land in this
exact narrow window far more likely on the cold path. Every subsequent
construction is warm (class already loaded) and essentially never hits it.
Confirmed directly: prepending a single throwaway, never-started
`new Thread(() -> {}, "warmup")` before the real workload eliminated the
bug in 5/5 runs.

## Fix

`native-builtins/src/lib.rs`, `populate_real_thread_holder`: re-read
`holder` through `holder_handle` (`ctx.read_native_pin(holder_handle,
holder)`) twice on the way in — once right after the pin (defensive,
covers the `ensure_class_initialized`/`alloc_object` window) and once more
immediately before `args` is built (the actual fix — covers the
group-resolution block). `target` (the user's `Runnable`, also pinned via
`target_handle`) gets the identical treatment at the same point, since it
was subject to the exact same "used raw before its own post-invoke
re-read" gap.

Also applied as defense-in-depth (confirmed independently, NOT sufficient
alone to fix this specific bug — this repro's defect is entirely inside
the native construction path described above, which never runs real
`putfield` bytecode for `task` at all): `vm/src/runtime/interpreter.rs`'s
`Instruction::Getfield`/`Instruction::Putfield` opcode handlers now run
their popped receiver `ObjectRef` through `shared.heap.load_and_forward()`
before use, matching the existing barrier already applied to every
native-call argument and to `Getfield`'s own loaded value. `resolve_field_ref`'s
cache-miss path is adjacent to both opcodes' receiver pop and can trigger
class-loading-driven allocation, so the same forwarded-but-unread-back
shape is theoretically reachable from plain interpreted bytecode too, even
though it was not what this specific repro hit.

## Verification

- Pre-fix: `CdlSpawnRepro` hung reliably on round 0 of every attempt
  (confirmed on a fresh build of `dev` tip `cce6e1c6`).
- Post-fix: 20/20 consecutive fresh-process runs clean
  (`CRATONVM_DBG_GC_STRESS=65536`), plus a further 10/10 at a 16x more
  aggressive stress interval (`CRATONVM_DBG_GC_STRESS=4096`) — all
  `finished=true count=0`, completing in under ~1.1s (vs. the pre-fix
  30,000ms timeout-then-hang).
- `cargo test -p cratonvm-vm --lib --release`: 2200 passed / 16-17 failed
  — the same pre-existing `jit::skip_list`/`runtime::lock_order` baseline
  this codebase's docs have documented repeatedly; independently
  reconfirmed present (byte-for-byte identical failure list) on a clean
  `git stash`-ed checkout of the same tip with no fix applied, ruling out
  any regression.
- `cargo test -p cratonvm-native-builtins --lib --release -- thread`:
  83/83.
- `cargo test -p cratonvm-vm --lib --release -- threading::`: 283/283.
- `cargo test -p cratonvm-vm --lib --release -- putfield getfield field_`:
  71/71 (1 ignored, requires real JDK on PATH — expected).

Files: `native-builtins/src/lib.rs` (fix, `populate_real_thread_holder`),
`vm/src/runtime/interpreter.rs` (hardening, `Instruction::Getfield`/
`Instruction::Putfield` receiver healing).

## Related

- `docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md`, section 5.8
  follow-up #7 — the primary writeup this file mirrors, with the fuller
  investigation narrative and the reasoning for why this is a distinct
  bug from follow-up #5 (`cce6e1c6`'s GC-barrier census-exclusion fix).
- `docs/internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md` — where
  this bug was originally found as a side-effect of an unrelated
  BigInteger AIOOBE investigation and spun off as a standalone follow-up.
- `b4ab3ad3` (`fix(vm): get_or_create_main_thread_group TOCTOU race +
  missing GC root`, landed the same day, unrelated repro) — a sibling bug
  in the same broad "Thread/ThreadGroup construction under a moving GC"
  territory, in a different function (`build_thread_field_holder`, used
  for VM-bootstrap thread mirrors rather than user `new Thread(...)`
  calls) with a different specific defect (a genuinely missing GC root,
  not an unread pin). Independently discovered and fixed; no code overlap
  with this fix.
