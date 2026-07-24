# RandomizedContext per-thread-null: fix evidence (2026-07-02)

Supporting evidence for
`docs/known-issues/elasticsearch-randomizedcontext-per-thread-null.md`. Raw
logs are not committed (repo-wide `*.log` gitignore, and they are mostly
noise) — the excerpts below are the parts that mattered.

## Repro command

See the "Repro" section of the known-issues doc. Direct `JUnitCore`
invocation of `org.elasticsearch.action.admin.cluster.stats.MappingStatsTests`
with `-Dtests.seed=B17AC9D3E1F2A0C4`.

## Pre-fix: deterministic single suite-level failure

```text
Time: 64.088
There was 1 failure:
1) org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null

FAILURES!!!
Tests run: 14,  Failures: 1
```

All 14 individual `@Test` methods pass; the failure is reported against the
*class* (JUnit's convention for a `@BeforeClass`/`@AfterClass`/rule-teardown
failure), after the last method (`testEqualsAndHashcode`) completes.

## Post pointer_map-completeness fix: same failure persists, plus an
occasional second one

Re-running the identical command against the fixed build reproduces the same
class-level failure **every time**. On some (not all) runs with the same
seed, a **second, non-deterministic** failure also appears mid-suite:

```text
E.[testSourceModes] before test
...
Time: 89.295
There were 2 failures:
1) testConcurrentSerialization(org.elasticsearch.action.admin.cluster.stats.MappingStatsTests)
java.util.concurrent.ExecutionException: java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
Caused by: java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
2) org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null

FAILURES!!!
Tests run: 14,  Failures: 2
```

The `ExecutionException` wraps a `Future.get()` inside
`testConcurrentSerialization` (the test explicitly exercises concurrency —
multiple worker threads), so this NPE fires on a worker thread mid-task, not
during teardown. This is the identical exception text as the deterministic
class-level failure, but a structurally different manifestation (mid-test,
non-deterministic, worker thread vs. teardown, deterministic, unidentified
thread).

## `CRATONVM_DBG_WATCHREF` trace analysis

Ran with `CRATONVM_DBG_WATCHREF=1`, which traces (a) every
`sweep_young_non_moving` invocation, (b) every watched-referent
identity-map insertion / dead-detection in the non-moving sweep, and (c)
every `process_weak_refs` KEEP/CLEAR decision.

Representative output from a run with the 2-failure (non-deterministic)
outcome:

```text
[watchref] publishing 605 watched referent(s): [1e9b2eb0, 1e97a6a0, ...]
[watchref] sweep_young_non_moving ENTRY (non-moving path taken)
[watchref] non-moving sweep: watched survivor kept in place @0x1e97a6a0 — identity-mapped
...
[watchref] non-moving sweep: watched address @0x1ecae4d0 was DEAD (unmarked) — zeroing
...
[watchref] weak CLEAR ref_obj=0x1ecaec08 referent=0x1ecae4d0
```

605 active Weak/Phantom references is the VM-wide count (JDK-internal
caches, reflection metadata, etc.) — not specific to RandomizedContext.

**Cross-referencing every `weak CLEAR` in the trace against every `"was
DEAD"` report**: every single cleared referent address was independently
reported dead by the sweep earlier in the same run (`comm -23` over the
sorted address sets produced zero unmatched entries). In other words, in
this run, `process_weak_refs` never cleared anything that wasn't already
confirmed genuinely dead — the pointer_map-completeness fix is working
correctly for what it targets.

This means: **the residual failure is not an "incorrectly cleared while
still alive" bug of the class this fix addresses.** Something else — not yet
identified — is producing the same NPE.

## Session 2 (2026-07-02, later): GC exonerated, narrowed to a JIT bug

Follow-up investigation after the pointer_map-completeness fix above did not
resolve the deterministic `MappingStatsTests` failure. Summary of the
evidence chain (full detail in the known-issues doc's "Residual: root cause
NARROWED to a JIT bug" section):

1. Added `[watchref] THREAD MIRROR IDENTITY CHANGED` tracing to
   `current_thread_object` — zero identity changes across 20 threads in a
   full failing run. Rules out mirror-address or identity-hash instability.
2. In the same trace, only ONE young GC ran in the entire ~38s/14-test run,
   and it happened before the suite thread's watched-referent address ever
   appeared in a published watch-list — i.e. before `RandomizedContext.
   create()` plausibly ran. No GC occurred in the relevant window at all in
   this run. GC cannot explain a failure with no GC present.
3. **`--nojit` does not reproduce the NPE** — it hits an unrelated
   `ClassCastException` in `testConcurrentSerialization` instead. This is the
   key finding: JIT compilation is *necessary* to reproduce the bug.
4. `CRATONVM_DBG_DUMP_JIT=LIST` on a failing run shows exactly four
   `java/util/WeakHashMap` methods get JIT-compiled: `hash`, `indexFor`,
   `matchesKey`, `maskNull` — precisely the bucket-lookup/key-match
   primitives behind `get()`/`getEntry()`. `matchesKey`'s compiled body is
   979 bytes (`CRATONVM_DBG_DUMP_JIT=matchesKey`), unusually large, with 3-4
   near-duplicate code blocks (not yet disassembled/verified).
5. Ruled out as the SPECIFIC cause: `CRATONVM_DISABLE_SCALAR_REPLACEMENT`,
   `CRATONVM_DISABLE_AALOAD_LICM`, `CRATONVM_DISABLE_ARITH_LICM`,
   `CRATONVM_DISABLE_UNROLL`, `CRATONVM_NO_IR_BRANCHY` — individually and all
   together, the NPE still reproduces. Not one specific, already-toggleable
   optimization pass.
6. Four isolated `.java` repros in `whm-repro` (single-threaded stress,
   interleaved resize, multi-threaded synchronized contention, thread
   start/join cycles) — none reproduced a single miss after millions of
   `get()` calls each. The bug needs either the ES workload's full scale (165
   total JIT-compiled methods, not just the four `WeakHashMap` ones) or a
   timing window these simplified repros don't hit.
7. Got a REAL disassembly via the existing (previously-undiscovered-this-
   session) `../../../../../vm/src/jit/disasm.rs` tool — `CRATONVM_DBG_JIT_DISASM=matchesKey`
   dumps NASM-formatted, address-annotated output (far better than the raw
   hex from `CRATONVM_DBG_DUMP_JIT`; see `matchesKey-jit-disasm.txt`).
   Cross-checked against real JDK 25 bytecode
   (`WeakHashMap-real-bytecode-excerpt.javap`, via `javap` on
   `<jdk>/lib/modules`): `matchesKey` is `e.refersTo(key) || (e.get() != null
   && key.equals(e.get()))`. The compiled code's three near-duplicate
   inline-cache blocks map exactly onto these three calls
   (`refersTo`/`get`/`equals`) with CORRECT high-level control flow — ruling
   out a wrong-branch/inverted-condition bug.
8. `e`/`key`/`k` are locals homed in callee-saved registers r12/r13/r14
   (confirmed against `../../../../../jit/src/x64.rs`'s documented "locals → callee-saved
   registers" convention and the observed save/restore prologue/epilogue).
   Each of the 3 blocks makes a GC-capable virtual-dispatch call and reloads
   afterward. `../../../../../jit/src/x64.rs` documents an adjacent, already-fixed hazard
   class right next to this exact code —
   `flush_callee_saved_oops_enabled` (default ON) — for operand-stack
   `CalleeSaved` entries specifically; unconfirmed whether plain
   local-variable homes get the same protection.
9. Tested `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` (blind-spills every GPR,
   not just callee-saved, at every safepoint) — did NOT fix it. Either this
   mechanism doesn't cover `matchesKey`'s call sites, or root cause isn't
   register-visibility-to-scanner at all.

### Repro commands for this session's findings

`--nojit` comparison (from `C:\craton\CratonVM\apps\elasticsearch`, same args
as the main Repro section, plus `--nojit`):

```powershell
& <cratonvm.exe> --java-home "C:\Program Files\Java\jdk-25" ... --nojit -cp $cp `
  org.junit.runner.JUnitCore org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
```

List every JIT-compiled method in a failing run:

```powershell
$env:CRATONVM_DBG_DUMP_JIT = "LIST"
& <cratonvm.exe> ... # same command, JIT on
# stderr contains one "[JIT_COMPILED] Class.methodName(desc)" line per compile
```

Dump one suspect method's machine code:

```powershell
$env:CRATONVM_DBG_DUMP_JIT = "matchesKey"
& <cratonvm.exe> ...
# stderr contains "[JIT_DUMP] ..." with the raw hex bytes
```

Isolated repros (`whm-repro/*.java`) — compile with `javac`, run under this
VM with `-cp .`, no special flags needed; they exercise `WeakHashMap` get/put
under JIT-compilation-forcing iteration counts, both single- and
multi-threaded. All four passed cleanly (0 misses) against the build that
still fails the full ES repro.

## Old-gen `is_addr_live` masking note

`GenerationalHeap::is_old_gen_addr(addr)` is a coarse region-bounds check
(`self.old_gen.lock().contains(addr)`), not a per-object liveness check —
*any* address within old gen's currently-allocated span reads as "live" for
`is_marked` purposes, regardless of whether that specific object is actually
reachable. This means the `OldGen::compact()` "stationary survivor" gap
(fixed alongside the young-sweep one, for general pointer_map-consumer
correctness) could not by itself explain an *incorrect clear* for an
old-gen-resident referent — `is_addr_live` already gives old-gen addresses
an unconditional pass for that specific check. Ruling this out is what
redirected the investigation away from "the suite thread's mirror got
promoted to old gen and hit the compaction gap" as an explanation for the
residual failure.

## Session 3 (2026-07-03): disassembly verification + bisection to a volume/timing effect

### Disassembly (real tool, not hex-by-eye)

Found `../../../../../vm/src/jit/disasm.rs` — an existing, previously-undiscovered-this-
investigation real x86-64 disassembler (`CRATONVM_DBG_JIT_DISASM=<Class.
method substring>`, NASM-formatted, address-annotated). Used it plus real
JDK 25 bytecode (`javap -p -c -classpath <jdk>/lib/modules
java.util.WeakHashMap`, see `WeakHashMap-real-bytecode-excerpt.javap`) to
verify all four suspect methods' compiled logic **instruction-by-instruction**
against real semantics. All four are correct:

- `matchesKey` (`matchesKey-jit-disasm.txt`, 979 bytes): `e.refersTo(key) ||
  (e.get() != null && key.equals(e.get()))` — the three near-duplicate
  inline-cache blocks map exactly onto `refersTo`/`get`/`equals`.
- `hash` (456 bytes): the full Wang/Jenkins-style mixing sequence
  (`h ^= (h>>>20)^(h>>>12); return h^(h>>>7)^(h>>>4);`) matches exactly,
  shift-by-shift.
- `maskNull` (116 bytes) / `indexFor` (89 bytes): trivial, both correct.

`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` (an existing, unrelated diagnostic
that blind-spills every GPR at safepoints, closing a *different* documented
hazard — see `flush_callee_saved_oops_enabled` in `../../../../../jit/src/x64.rs`) was
tested and did NOT fix the bug.

### New diagnostic: `CRATONVM_JIT_DENY`

Added to `../../../../../jit/src/lib.rs` (in `try_compile`, right after the existing
bail-list check): `CRATONVM_JIT_DENY=<comma-separated Class.method
substrings>` force-interprets matching methods while everything else still
JIT-compiles normally. Runtime env var, no rebuild between experiments.

### Bisection result: it's not one method

A failing run JIT-compiles 128 distinct methods (`all-compiled-methods.txt`,
full list). Binary-searched by denying successively smaller subsets:

```
matchesKey alone (1)                    → still fails
all 4 WeakHashMap methods (4)            → still fails
methods 1-64 "half1" (64)                → FIXED
  methods 1-32 "q1" incl. ThreadLocal*   → still fails
  methods 33-64 "q2"
    methods 33-48 "e1" incl. all 4 WHM   → still fails
    methods 49-64 "e2"
      methods 49-56 "f1"                → still fails
      methods 57-64 "f2"
        methods 57-60 "g1"              → still fails (2 failures)
        methods 61-64 "g2"              → still fails
methods 65-128 "half2" (64, DISJOINT from half1) → FIXED (same as half1)
```

The decisive result: **two completely disjoint 64-method sets each
independently fix the bug**, while every smaller subset tried (32, 16, 8, 4
— including ones containing all 4 originally-suspected `WeakHashMap`
methods, and including the entire `ThreadLocal`/`ThreadLocalMap` cluster)
does not. No single method can belong to two disjoint sets, so this
conclusively rules out "one specific miscompiled method" as the
explanation. The dependent variable is the *amount* of JIT-compiled code
active — a volume or timing effect, not a per-method correctness bug.

**Reframing**: this is now best understood as a genuine race condition
whose window's probability scales with execution speed/interleaving, which
JIT compilation volume directly affects. Consistent with the
previously-observed non-deterministic `testConcurrentSerialization`
mid-test failure (same NPE, some runs only) — same underlying race,
different landing spot depending on timing. Next steps should target TIME
(artificial delays at specific points in the call chain) rather than WHICH
CODE, to independently confirm the timing-window theory without going
through JIT at all.

## 2026-07-04 follow-up: hash-code corruption and Elasticsearch package skip

Current `dev` no longer reproduces this issue under `--nojit`; `MappingStatsTests` passes all 14 tests there. The remaining failure is JIT-only.

A direct JIT-on run first re-hit the hash/equality helper cluster: the compiled-method list stopped at `java/util/Arrays.hashCode([Ljava/lang/Object;)I`, repeated `FieldScriptStats.hashCode()I`, and `java/util/Objects.hash([Ljava/lang/Object;)I`, followed by either a wrong hash-code result, invalid stream/vInt data, or a crash in `VmHeap::flush_thread_satb` reached from `jit_anewarray_object`. The crash-side object dump also showed an out-of-bounds field read against `java/lang/Long`, which is consistent with earlier object-layout corruption rather than a standalone SATB helper bug.

Masking only Elasticsearch stats methods did not fix the suite. Masking `java/,jdk/` removed the hash/crash symptom but left stream serialization failures. The deterministic passing mask was:

```text
CRATONVM_JIT_DENY='java/util/Objects.hash,java/util/Objects.equals,java/util/Arrays.hashCode,jdk/internal/util/ArraysSupport.hashCode,org/elasticsearch'
```

That run completed `org.elasticsearch.action.admin.cluster.stats.MappingStatsTests` with `OK (14 tests)`. A narrower mask that skipped the hash/equality cluster plus `org/elasticsearch/action/admin/cluster/stats` and `org/elasticsearch/TransportVersion` still returned the original class-level `RandomizedContext.getPerThread() == null` failure, so the deterministic mitigation has to keep Elasticsearch application classes interpreted under the conservative policy until that package can be bisected safely.

The code fix therefore keeps the known hash/equality corruption cluster skipped unconditionally under `SkipPolicy::Conservative`, independent of the callee-saved-GPR-local-homes opt-in, and adds a conservative Elasticsearch package skip. Both skips remain liftable through `CRATONVM_JIT_ALLOW_PACKAGES` for future bisection.
