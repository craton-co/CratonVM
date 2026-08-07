# `TestOutOfMemory` crashed the whole process (`SIGABRT`) instead of throwing a catchable `java.lang.OutOfMemoryError`

## Status
**FIXED** (2026-07-31, branch `fix/h2-testoutofmemory-abort-20260731`). The
abort is gone: heap exhaustion inside a native allocation now raises the same
catchable `java.lang.OutOfMemoryError` HotSpot throws, and the VM stays usable
afterwards. Verified by a probe whose output is line-for-line identical to
HotSpot JDK 25, and by a new regression test
(`vm/tests/native_alloc_oom_catchable.rs`, jit + nojit).

The class still does not *pass* — see "Residuals" below; none of them is this
crash, and one of them (HotSpot itself fails this class at `--Xmx 1g`) means
"passes" was never the right bar.

## Original symptom
```
FATAL: OutOfMemoryError: young gen exhausted — tried to allocate 53087104 bytes,
  from-space has 485786952/536870912 used
#  SIGABRT at pc=0x785b3129ec0c, ...
```

## What the investigation actually found

### 1. HotSpot does NOT pass this class at `--Xmx 1g` either
(the doc's open question #1, answered)

```
20:02:31 org.h2.test.db.TestOutOfMemory Failure
java.lang.AssertionError: Failure
  at org.h2.test.db.TestOutOfMemory.testDatabaseUsingInMemoryFileSystem(TestOutOfMemory.java:133)
```
`java -Xmx1g` (JDK 25, this host), 5s, rc=1, run twice. Line 133 is the
`conn.close(); fail();` assertion — `close()` does not throw. So the target is
**"fail like HotSpot fails", not "pass"**: what mattered was that the failure be
a Java-level one instead of a process abort.

### 2. The abort site, named
(the doc's open question #3, answered — and it *was* the already-known fallback)

A temporary `CRATONVM_DBG_OOM_BT=1` backtrace at the abort (release builds carry
line tables) gave the exact chain:

```
report_fatal_oom                gc/src/gen_heap.rs
alloc_array                     gc/src/gen_heap.rs:1350
VmHeap::alloc_array             gc/src/vm_heap.rs:225
NativeContextImpl::new_array    vm/src/vm/vm_exec.rs:7795
s2_bb_alloc                     native-builtins/src/servlet.rs:2679   <-- ByteBuffer.allocate(int)
register_s2_bytebuffer::{{closure}}
safe_native_call_impl           vm/src/vm/vm_exec.rs:986  (inside catch_unwind)
```

`java.nio.ByteBuffer.allocate(int)` is force-registered as a native (note in
passing: it is intercepted even in real-JDK mode, and builds a *synthetic*
`java/nio/ByteBuffer` rather than running the JDK's own bytecode — a separate
question about native-override surface, not touched here). H2's
MVStore-on-`memFS` workload calls it with ~76 MB, which is below the humongous
threshold (50% of a young semi-space), so it took the young path, found young
full, spilled to old gen, found old gen full — and hit the abort that
`docs/internal/fixed-suite-bugs/tomcat/02-native-young-gen-oom-abort-FIXED.md`
had explicitly left in place ("Falls through to the original abort only if old
gen is also full"). The doc's own guess was right.

The improved diagnostic (kept — see "Fix" #4) printed the whole picture:
```
FATAL-OOM detail: young_to 0/1073741824 used; old_gen 536870456/536870912 used
  (largest free block 456, 1 free blocks); minor_gc=24 major_gc=10
  promoted_bytes=699235248 old_allocs=6 young_allocs=518307
```
Old gen was **genuinely** full (largest free block: 456 bytes) after 699 MB of
promotion into a 512 MB, non-expandable old generation. GC was running (24 minor
/ 10 major cycles), so this was not a "heap full of garbage that never got
collected" case.

### 3. The heap is OVER-provisioned at this `-Xmx`, not under-provisioned
(the doc's open question #2, answered — and the answer is the opposite of the guess)

At `--Xmx 1g` the numbers above show `young_from` at 512 MB **and** `young_to`
at 1024 MB, plus a 512 MB old gen. Measured RSS of the `--Xmx 1g` process:
**4.0 GB** (`total-vm 7.5 GB`). `-Xmx` bounds only the *initial* split; the
young semi-space may then expand up to `MAX_HEAP_EXPANSION_FACTOR` (4x) with no
reference to the total budget. This is a real, separate defect — it got this
very test killed by the Linux OOM killer on a shared host — and it is now
tracked in `docs/known-issues/gc-young-semispace-expansion-ignores-xmx.md`.
It is **not** the cause of the abort, and fixing it is a GC-wide change with a
much larger blast radius than this record, so it was deliberately not bundled
here.

## Root cause

`NativeContext::new_array` / `new_ref_array` / `alloc_object` funnel into the
**panicking** `GenerationalHeap::alloc_array` / `alloc_object`. Those entry
points cannot GC-and-retry — a native holds raw `ObjectRef`s in Rust locals that
are in no GC root set — so on double exhaustion they `std::process::abort()`.

Three earlier members of this family were fixed **one call site at a time** by
threading the fallible `try_new_array` / `try_new_ref_array` allocators through
the offending native (`docs/internal/gaps/crash-01-arraylist-capacity-oom-abend.md`,
`crash-02-native-capacity-ctor-abort-family.md`, and `Cipher.doFinal`).
`ByteBuffer.allocate` was the fourth. With ~1000 `ctx.new_array(..)` call sites
in `native-builtins`, that approach does not converge — hence a generic channel.

## Fix

1. **`vm/src/runtime/native_oom.rs` (new)** — a *scoped* heap-exhaustion unwind
   channel. A native callback dispatched by `safe_native_call_impl` runs
   directly under that function's `catch_unwind`, with **no JIT-compiled frame
   in between**, so a Rust unwind from there is safe and is already an
   established recovery path. On true double exhaustion the allocators unwind
   with a `NativeAllocOom` payload and the boundary converts it into
   `RuntimeError::OutOfMemoryError`, which the interpreter materialises as a
   real, catchable `java/lang/OutOfMemoryError` (falling back to the
   pre-allocated `singleton_oom` when the heap is too full to build one).

   The permission is scoped because JIT frames carry no unwind information — a
   panic that has to cross one terminates the process (the same constraint that
   makes `jit_throw_aioobe` signal through a thread-local instead of panicking),
   and `NativeContextImpl` is also constructed *inside* JIT helpers. So:
   `safe_native_call_impl` grants the permission around the callback, and
   `JitEntryGuard` — the guard every JIT/OSR entry constructs — suspends it for
   the duration of the compiled frame. A native invoked *from* JIT code is still
   covered: the unwind stops at `safe_native_call`'s handler without ever
   reaching the compiled frame.

   Cost: the JIT-entry side is one thread-local read and **no write** when no
   native call is in flight (the common case); the native-call side is one read
   plus two writes per call.

2. **`vm/src/vm/vm_exec.rs`** — `NativeContextImpl::new_array` / `new_ref_array`
   and the two terminal object allocations in `alloc_object` take the fallible
   `try_alloc_array_full` / `try_alloc_object_full` twins when the permission is
   held. Those twins walk the *identical* no-GC young -> old-gen spill path, so
   every allocation that fits is byte-for-byte unchanged; only true exhaustion
   diverges. Outside a native callback the historical abort is retained.

3. **`native-builtins/src/servlet.rs`** — `s2_bb_alloc` (`ByteBuffer.allocate`)
   additionally uses the fallible `ctx.try_new_array` and returns a catchable
   OOME directly, so the site that started this does not depend on the unwind.

4. **`gc/src/gen_heap.rs`** — `report_fatal_oom`: the surviving abort now prints
   young-to, old-gen occupancy, old-gen largest free block and block count, and
   the GC counters, plus (under `CRATONVM_DBG_OOM_BT=1`) the allocating
   backtrace. The old one-line message named only the young from-space, which
   reads as "young gen too small" even when the blocker is a full old gen —
   exactly the misreading this record's original author had to guess around.

5. **panic hooks** (`vm/src/runtime/crash_handler.rs`, `vm-cli/src/main.rs`) —
   both recognise the `NativeAllocOom` payload and stay quiet. Writing an
   `hs_err_pid<pid>.log` for a *handled* condition would be wrong on its own and
   would also latch the crash handler's one-report-per-process guard, silently
   suppressing the report for a later real crash.

## Validation

`vm/tests/native_alloc_oom_catchable.rs` (new, jit + nojit): both pass.

Probe (`ByteBuffer.allocate(Integer.MAX_VALUE-8)`; fill with `byte[]`; fill with
`ByteBuffer`s; usability check after each), `-Xmx 512m`:

| line | HotSpot JDK 25 | CratonVM (before) | CratonVM (after) |
|---|---|---|---|
| `1 BB_HUGE` | `CAUGHT OutOfMemoryError` | **`SIGABRT`** | `CAUGHT OutOfMemoryError` |
| `1b ALIVE` | `bb=42 cap=4096 arr=7 sb=190` | — | `bb=42 cap=4096 arr=7 sb=190` |
| `2 ARR_FILL` | `CAUGHT OutOfMemoryError` | — | `CAUGHT OutOfMemoryError` |
| `2b ALIVE` | `bb=42 cap=4096 arr=7 sb=190` | — | `bb=42 cap=4096 arr=7 sb=190` |
| `3 BB_FILL` | `CAUGHT OutOfMemoryError` | — | `CAUGHT OutOfMemoryError` |
| `3b ALIVE` | `bb=42 cap=4096 arr=7 sb=190` | — | `bb=42 cap=4096 arr=7 sb=190` |
| `DONE` | yes | — | yes |

Identical to HotSpot on every line, with jit and with `--nojit`; the baseline
build aborts on the first one in both modes.

`TestOutOfMemory` itself no longer aborts — it runs on past the OOM into the
`testDatabaseUsingInMemoryFileSystem` insert loop.

## Residuals (each attributed; none is this crash)

* **`STW cross-thread JIT takeover is still waiting for cooperative mutators
  rounds=64 pending=1 taken=0`** — the second symptom quoted in the original
  record. **Benign here.** A `CRATONVM_DBG_STW_CENSUS=1` run shows, at every
  single warning, all threads `ready=true`, no `[gcbarrier-tripwire]` mismatch,
  and an `[stw-arrive] ... arrived=1 expected=1` immediately after. `expected`
  is 1 and the one outstanding mutator is a real, running thread (H2's
  serialization/`MVStore background writer` worker) that simply takes longer
  than the 64 x 1 ms warn threshold to reach a safepoint while copying multi-MB
  buffers. This is arrival latency under heavy allocation, not the unarriving
  -mutator livelock of
  `docs/internal/fixed-suite-bugs/stw-crossthread-jit-takeover-hang-cluster.md`.

* **The class exceeds the suite timeout.** Baseline and fixed builds both spend
  their time in the same place. A `--stack-dump-on-timeout 420` run on the
  **unmodified baseline** shows the main thread inside
  `CreateTable.insertAsData -> Insert.insertRows -> Select$LazyResultQueryFlat
  .fetchNextRow -> StringFunction1.getValue`, i.e. the
  `create table ... as select x, space(1000000+x) from system_range(1, 10000)`
  insert loop, progressing (frames differ between samples). That is the
  pre-existing, separately-tracked MVStore insert/commit throughput cliff —
  `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md` in this directory, which already
  calls itself "likely the dominant cause of the CratonVM-specific silent hangs".
  Not introduced here, and not fixable in this record.

* **Committed heap exceeds `-Xmx`** — see section 3 above. **FIXED**
  2026-07-31 in a follow-up (`fix/xmx-heap-budget-20260731`): `-Xmx` now bounds
  `young_from + young_to + old_gen`, and this test's peak RSS at `--Xmx 1g`
  drops from 3.63 GB to 2.09 GB with the young semi never expanding. Retired to
  `docs/internal/fixed-suite-bugs/gc-young-semispace-expansion-ignores-xmx-FIXED.md`.

* **`SIGSEGV addr=0x0` in class-mirror slot resolution** — a *pre-existing*,
  heap-pressure-dependent crash this class reaches after ~9-13 minutes, in
  roughly one long run in two to three. Reproduced on **unmodified `dev`** at
  `--Xmx 512m` with a byte-identical register signature, so it is neither this
  record's abort nor the heap-budget follow-up. **FIXED** 2026-07-31 in a follow-up: the GC own corrupt-header
  diagnostic Debug-formatted an `ObjectKind` whose byte was not a valid
  discriminant, so the derived `Debug` read a `&str` past the end of the
  variant-name table. Retired to
  `docs/internal/fixed-suite-bugs/gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md`.

* **`gen_heap::set_field: out-of-bounds field write dropped` on a
  `java/lang/Object` with `num_slots=0`** — appears in the post-OOM region that
  the baseline never reached (it aborted first). The write is dropped by the
  guard; this is the pre-existing `RESID-DIAG` residual already instrumented in
  `gen_heap.rs` (dohead residuals investigation, 2026-07-18), not a new defect.

## Repro (original, pre-fix)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestOutOfMemory
```
Non-deterministic: aborted at ~233s in one run, survived past 420s in another.
The `OomProbe`/`vm/tests/native_alloc_oom_catchable.rs` probe reproduces the
same abort in ~10 seconds and deterministically, and is the right regression
vehicle.
