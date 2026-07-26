# G1 backend could OOM-abort mid-boot/mid-workload because native methods that allocate internally never triggered a GC safepoint check

## Status
**FIXED 2026-07-26** (branch `fix/g1-native-alloc-safepoint-20260726`).
Root-caused 2026-07-25 (history preserved below), fixed and verified
2026-07-26. The repro this doc opened with now runs indefinitely with a
healthy collection cadence instead of aborting within 1-10 seconds.

This was a **general VM/GC architecture gap**, not specific to H2 or
`--nojit`: it affected the G1 backend under ANY workload whose hot path
called native methods that allocate heap objects internally without any
interleaved bytecode-level allocation instruction.

## Reproduction (pre-fix)
```
cd apps/h2database-suite-runner   # H2_ROOT pointed at a checkout of apps/h2database/h2
CP=<h2 test classpath>            # target/classes:target/test-classes:<m2 test-scope classpath>
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm-bin> --java-home <jdk25> \
  --Xmx 1g --nojit -c "$CP" org.h2.test.unit.TestFileSystem
# pre-fix  -> FATAL: G1: out of heap space for object allocation (40 bytes), SIGABRT
#             within 1-10s; --Xmx 4g/8g only delayed it.
# post-fix -> no abort; runs on (the class itself still exceeds the suite
#             runner's watchdog for the unrelated, still-open performance
#             reason its sibling doc covers).
```
`--nojit` is the trigger only because it is the ONE thing that flips the
default GC backend from `Generational` to `G1`
(`vm-cli/src/main.rs` ~line 2351, "Interpreter-only workloads..." comment) —
the bug lived entirely in `gc/src/g1.rs`, not in anything JIT-related. An
explicit `-XX:+UseG1GC` reproduced it with the JIT on as well.

## Root cause (confirmed via instrumentation + live gdb)
`maybe_gc()` (`vm/src/runtime/interpreter.rs`), the ONLY call site that
ever triggers `G1Collector::collect_garbage()` during normal execution, is
invoked from exactly 5 places in the interpreter — all 5 are **bytecode-level
allocation instructions**: `New`, `Newarray`, `Anewarray`, `Multianewarray`,
and the array-via-constructor-reference special case. There is **no**
`maybe_gc()` call after a native method invocation returns, no matter how
much heap that native method allocated internally via `ctx.new_object`/
`alloc_synthetic`-style helpers.

Consequence: a hot loop whose body calls only native methods (no bytecode
`new`/`newarray` of its own — e.g. `AsynchronousFileChannel.read(buf, pos)`
in a tight loop, where every allocation happens INSIDE
`native_afc_read`/`afc_box_integer` on the Rust side to box the return
value) can run for many thousands of iterations with **zero** GC safepoint
checks. Each iteration's small boxed-Integer garbage silently piles up.
Eventually `G1Collector::alloc_object` (`gc/src/g1.rs`, the infallible
`GarbageCollector` trait entry point — it returns `ObjectRef`, not
`Option`/`Result`, so it cannot report failure and therefore cannot force a
GC and retry) exhausts every region and calls `std::process::abort()` — with
no opportunity for ANY collection to run in between, because nothing ever
asked for one.

Note that the *interpreter's* allocation paths were never at risk:
`alloc_object_shared`/`gc_alloc_array` (`interpreter.rs`) use the FALLIBLE
`try_alloc_object`/`try_alloc_array`, force a GC on failure, retry, run
`g1_force_full_cycle` as a last ditch, and finally raise a catchable Java
`OutOfMemoryError`. Only the infallible entry point — reached from
`NativeContextImpl::alloc_object`/`new_array` (`vm/src/vm/vm_exec.rs`) and a
handful of VM-internal bootstrap allocations — could abort.

This was invisible under the default `Generational` backend only because
`GenerationalHeap::alloc_object` has an internal, self-contained escape
hatch: "on \[young-gen\] exhaustion spill to old gen (non-moving) BEFORE the
hard abort" (`gc/src/gen_heap.rs`), plus the `young_spill_pressure` latch
described below. G1 had neither.

### Evidence trail
1. `--Xlog "gc*=info:stdout:..."` and `CRATONVM_GC_STATS=1` (both real,
   wired flags — see `vm-cli/src/main.rs` `--Xlog`/`--verbose:gc`) showed
   **zero** GC-related output before the abort on the fast (`--Xmx 1g`,
   OOMs in <1s during VM bootstrap) repro — misleading at first (looked like
   "GC never runs at all"), but this specific fast repro just never got far
   enough into user code to hit a `maybe_gc()` checkpoint at all yet.
2. `CRATONVM_DBG_G1DIAG=1` (region-type counts before/after every
   `collect_garbage()` call — now a permanently wired flag, see
   `types/src/flags.rs`) on an `--Xmx 1g` run showed **5 healthy
   collections**, each correctly freeing/recycling regions (`free`
   oscillating between ~980 and ~1020 out of 1024 regions, `pinned=0`
   throughout — ruling out a JNI-critical-pin leak, and ruling out the
   previously-fixed "kept-region death spiral" from `docs/known-issues`
   history / `[[g1-serial-defect-stack-steadychurn]]`, since there were zero
   evacuation-failure `[RETRY]` events and zero permanently-stuck regions
   between collections).
3. Between the 5th collection's completion (`free=1020, eden=1`) and the
   abort, `free` went from 1020 straight to **0** with **no further
   `collect_garbage()` call in between** — i.e. ~1020 MB of straight-line
   allocation with zero safepoint checks. Re-confirmed verbatim on
   2026-07-26 against `dev` `95e4d9929`:
   ```
   DBG-G1DIAG-PRE:  collection #4 regions=1024 free=1020 eden=1 survivor=2 old=0 pinned=0
   DBG-G1DIAG-POST: objects_copied=10523 bytes_copied=1164888 bytes_freed=1897816
   DBG-G1DIAG: ... free=0 eden=1022 survivor=2 old=0 hstart=0 hcont=0 collection_count=5
   FATAL: G1: out of heap space for object allocation (40 bytes)
   ```
4. `gdb -batch -ex 'break abort' -ex run -ex 'bt 45'` on the live repro
   caught the exact stack at the moment of abort (re-confirmed 2026-07-26):
   ```
   #3  {closure#0} ()                  at gc/src/g1.rs:7063   (eprintln!+abort)
   #5  alloc_object ()                 at gc/src/g1.rs:7031
   #6  alloc_object ()                 at gc/src/vm_heap.rs:214
   #7  alloc_object ()                 at vm/src/vm/vm_exec.rs:6168
   #8  alloc_synthetic ()              at native-io/src/lib.rs:15349
   #9  afc_box_integer ()              at native-io/src/lib.rs:15915
   #10 native_afc_read ()              at native-io/src/lib.rs:15824
   #11 {closure#4} ()                  at vm/src/vm/vm_exec.rs:950
   ...
   #20 execute_invokevirtual_cached () at vm/src/runtime/interpreter.rs:40554
   ```
   Confirms: the failing allocation is `afc_box_integer` (boxing
   `AsynchronousFileChannel.read`'s int return value), invoked via ordinary
   cached-invokevirtual bytecode dispatch — exactly the "native call inside
   a loop with no bytecode `new`" shape predicted above.

## The fix

The 2026-07-25 write-up proposed adding `maybe_gc()` at five or six
interpreter invoke-dispatch call sites, and explicitly rejected doing it
inside the shared `safe_native_call`/`safe_native_call_impl` wrapper
(`vm/src/vm/vm_exec.rs`) because that wrapper also serves *nested*
re-entrant native calls.

**That reasoning turned out to be superseded by code that already exists.**
`safe_native_call_impl` ALREADY runs an orchestrated `maybe_gc_forced_pub`,
at the one point on the native-dispatch path where every Java argument is
pinned in `thread.native_pin_roots` and remapped afterwards — precisely so a
moving collection is safe there. It is gated on
`VmHeap::young_spill_pressure()`, a latch the **generational** heap sets when
an allocation wrapper had to spill young→old, i.e. "the mutator is outrunning
me and I could not collect from where I noticed". `VmHeap::young_spill_pressure()`
returned a hardcoded `false` for G1, so G1 never used that machinery at all.

The fix is therefore to give G1 its half of that existing protocol rather
than to open five new safepoints on the hottest dispatch paths in the VM:

1. **`G1Collector::native_alloc_pressure`** (`gc/src/g1.rs`) — a latch set
   whenever a NEW region is claimed for Eden (`alloc_in_region` /
   `refill_tlab`) and the remaining Free pool has fallen below the same
   threshold `needs_gc()` uses (25% of regions, adaptive). The count is
   O(num_regions) but runs only on a region claim — once per `region_size`
   (1 MiB) of allocation, never per object. Cleared by the consumer and at
   the end of every collection.
2. **`VmHeap::young_spill_pressure`/`clear_*`/`note_*`** (`gc/src/vm_heap.rs`)
   now delegate to that latch for the `G1` arm. No VM-layer change at all:
   `safe_native_call_impl`'s existing block picks it up, re-checks
   `needs_gc()` and `gc_overhead_limit_exceeded()`, and runs the collection.
3. **A TLAB emergency reserve** (`G1Collector::tlab_reserve_regions`) — the
   doc's own next-step #4, in the one shape that is provably safe. A TLAB
   refill is a *speculative bulk* claim (the thread may retire the chunk with
   most of it unused), so once the Free pool is down to the reserve
   (`num_regions/64`, at least 2, capped at `num_regions/8` so small heaps
   are never starved) `refill_tlab` steps aside and latches the pressure
   signal. `refill_tlab` returning `None` is an already-handled path — the
   caller falls back to per-object allocation — so this cannot wedge
   allocation; it just keeps the last regions for the allocator whose callers
   cannot be told "no".
4. The two `abort()` messages now name the invariant they encode, so the
   next core dump is readable.

Nothing outside the G1 backend changed. Under the default `Generational`
collector every one of these paths is byte-for-byte the code that shipped
before.

## Verification

* **The doc's own repro** (`TestFileSystem`, `--nojit --Xmx 1g`): pre-fix
  `SIGABRT` within 1-10 s (reproduced on `dev` `95e4d9929`); post-fix no
  abort at all. `CRATONVM_DBG_G1DIAG=1` shows a healthy steady state —
  collections trigger at `free=255/1024` (the 25% threshold) and free
  ~800 MB each while copying ~1 MB of live data, confirming the heap really
  was ~99.9% garbage at the moment the pre-fix binary aborted.
* **`gc` crate unit tests**: 867 lib tests + all integration targets green,
  including three new regression tests
  (`g1::tests::native_alloc_pressure_arms_only_once_the_free_pool_falls_below_the_gc_threshold`,
  `..._rearms_after_clear_and_is_cleared_by_a_collection`,
  `tlab_refill_holds_back_an_emergency_reserve_for_real_allocations`).
* **Fast regression suite** (`regression-suite/run.sh`) in FOUR configs —
  default (Generational + JIT), `--nojit` (G1), `-XX:+UseG1GC` + JIT, and
  the same three against the unpatched baseline binary: identical results in
  all six runs (9 passed / 2 failed; `RCollections` and `RReflect` fail
  identically on the unpatched `dev` binary — pre-existing, unrelated).
* **`vm` crate unit tests**: 2426 passed / 1 failed; the single failure
  (`runtime::interpreter::wave1_adoption_tests::sharing_one_memo_cell_across_two_triples_silently_mis_answers`,
  a native-call-site memoization assertion) reproduces identically with this
  branch's changes `git stash`ed on the same worktree — pre-existing,
  unrelated.
* **H2 suite, `nojit-real` mode — i.e. the G1 backend — all 218 classes**,
  patched vs. unpatched baseline, 4 shards, 180 s per class:

  | | PASS | HANG | FAIL |
  |---|---|---|---|
  | unpatched `dev` `95e4d9929` | 147 | 52 | 19 |
  | with this fix               | **148** | 51 | 19 |

  **Zero G1 heap-exhaustion aborts with the fix** (grep for "out of heap
  space" across all 218 per-class logs); the unpatched run has exactly one,
  `TestFileSystem` — this doc's repro.

  Seven classes changed status. Every one was chased down individually:

  | class | base → fix | verdict |
  |---|---|---|
  | `unit.TestFileSystem` | FAIL → HANG | **the fix.** FAIL *was* the OOM abort; it now runs and instead hits the sibling doc's performance ceiling. |
  | `store.TestStreamStore` | FAIL → PASS | improvement (base failed on an unrelated `sun.nio.ch.Interruptible` NPE at 15 s). |
  | `unit.TestKeywords` | HANG → PASS | timing: 180.2 s (over the cutoff) vs 166.4 s (under it). |
  | `db.TestLargeBlob` | FAIL → HANG | both non-PASS; base failed on an unrelated MVStore IO exception. |
  | `mvcc.TestMvccMultiThreaded` | PASS → FAIL | **flake, not a regression:** re-ran 4x per binary in isolation — 4/4 clean on BOTH. The suite failure was `JdbcSQLTimeoutException: Timeout trying to lock table` after 2.1 s under 4-shard parallel load. |
  | `store.TestMVStore` | HANG → FAIL | **pre-existing:** the same `AssertionError: Expected: ! Actual: null` reproduces under `--nojit -XX:+UseGenerationalGC`, where this change is provably inert (the `G1Collector` is never even constructed). The fix only lets the class reach it before the 180 s cutoff. |
  | `store.TestRandomMapOps` | HANG → FAIL | **pre-existing, same proof:** `ClassCastException: java.lang.Object cannot be cast to java.lang.Integer` reproduces 2/2 under `--nojit -XX:+UseGenerationalGC`. Under the *unpatched* G1 binary this class makes essentially no progress at all (1 progress line in 900 s, vs. ~2500 random ops in ~3 min with the fix) — a second, independent symptom of the same "G1 never collects" gap. |

## Notes for future work

* `--nojit` selecting G1 means every interpreter-only run of any suite
  exercises this path. The pressure latch fires only under real occupancy
  pressure (Free pool under 25%), so a comfortable heap pays nothing beyond
  one relaxed atomic load per native dispatch — the load that was already
  there for the generational latch.
* The residual "`TestFileSystem` exceeds the 300 s watchdog" behaviour is a
  DIFFERENT, still-open issue with its own doc:
  `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`
  ("Open: remaining performance gap"). It is a flat, distributed performance
  ceiling, not this OOM.
* This workload allocates ~800 MB of Java-heap garbage per GC interval
  entirely from inside `native_afc_read` (one `Integer` box plus one
  completed-future wrapper per `AsynchronousFileChannel.read`). Cutting that
  allocation rate is an obvious follow-up for the sibling doc's performance
  investigation, now that the allocations are actually collectable.

See `[[g1-serial-defect-stack-steadychurn]]` for the PREVIOUSLY fixed,
superficially-similar "kept-region death spiral" (ruled out here — no
evacuation failures observed).
