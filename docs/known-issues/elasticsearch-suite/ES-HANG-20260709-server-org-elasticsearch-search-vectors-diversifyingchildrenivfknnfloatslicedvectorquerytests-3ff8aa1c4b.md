# 2026-07-13 follow-up: array-constructor-reference lambda bug found + fixed
# (real, verified, but NOT yet confirmed as this doc's residual root cause)

While independently re-investigating this cluster (parallel to, and without
visibility into, the "2026-07-13 current-dev residual update" section
immediately below until after landing this fix), found and fixed a real,
previously-unknown interpreter correctness bug: **array-constructor-reference
lambdas (`SomeType[]::new`, used as an `IntFunction<SomeType[]>` — the
mechanism behind `Collection.toArray(SomeType[]::new)`, ubiquitous since
Java 11, and any direct user code) allocated a corrupted zero-field pseudo-
object instead of a real array.**

Root cause: `MethodHandleKind::NewInvokeSpecial` (the constructor-reference
lambda dispatch, `vm/src/runtime/interpreter.rs` and its duplicate in
`vm/src/vm/vm_exec.rs`) unconditionally treated the impl handle's target as a
regular class — allocate `num_total_fields` object slots, dispatch `<init>`.
For an array-shaped impl class name (e.g. `"[Ljava/nio/ByteBuffer;"`),
`load_class` correctly resolves it to the synthesized array `ClassId` (JVMS
5.3.3), but arrays have neither fields nor a constructor: the old path
allocated a zero-field object wearing the array's `ClassId`, silently
discarded the requested length (the `IntFunction`'s sole `int` argument), and
produced a value that fails a later `checkcast` to the real array type.

This exact mechanism reproduces from real Lucene: `ByteBuffersDataInput`'s
constructor does `this.blocks = list.toArray(ByteBuffer[]::new)`, immediately
followed by `checkcast [Ljava/nio/ByteBuffer;`. A direct, isolated repro
(`new ByteBuffersDataInput(list)` against the real `lucene-core-10.4.0.jar`,
bypassing the whole ES/suite-runner harness) threw
`ClassCastException: java.lang.Object cannot be cast to [Ljava.nio.ByteBuffer;`
on unfixed `dev`, and one *direct* (non-suite-runner) repro of this doc's own
`testSlicesDense` — bypassing the outer watchdog entirely — surfaced this
identical exception in ~85s instead of the usual multi-hundred-second
non-progress, i.e. this bug is a real, independent cause of SOME of this
cluster's non-deterministic behavior (fast completion with a wrong-data
exception, vs. the more common very-slow/non-progress runs), not necessarily
of every manifestation.

**Fixed** (commit on `fix/gc-stw-monitor-race-20260711-local`, merged to
`dev`): detect the array case via the resolved `ClassId`'s `array_info`
before the object-allocation path, and allocate a real array — primitive or
reference, correct component type, correct length — the same way
`anewarray`/`newarray` do for the same class metadata. Verified: the real
`ByteBuffersDataInput` construct + `readByte()` + `slice()` repro now matches
real JDK 25 output exactly. `cratonvm-vm --lib` lambda-proxy/lambda-dispatch
suite 8/8 pass; full `cratonvm-vm --lib` 2185 passed / 23 failed, all 23
pre-existing/environmental (debug-only lock-order assertions that cannot fire
in a release build, JIT skip-list feature tests, JNI table-size tests,
real-JDK-detection tests) and unrelated by name/code-path to this change.

**Open question for whoever picks this doc back up next:** does this fix
change the outcome of the "2026-07-13 current-dev residual update" repro
below (`testSlicesSparseWithFilter`, `others.tsv -Start 2538`,
`ByteBuffersIndexInput.slice`/`MockIndexInputWrapper.slice` non-progress)? A
corrupted zero-field pseudo-array being read back as if it had a real array
length header (garbage/huge length) is mechanistically consistent with
"consumes one CPU continuously, fails to produce a result before a 90s
timeout" — but this was NOT confirmed against that exact repro (host
unavailable for the remainder of this session). Re-run that doc's exact
repro command against a build including this fix before doing further
investigation into `ByteBuffersIndexInput`/`TaskExecutor` specifically — if
this fix resolves it, this cluster's genuine remaining root cause was never
GC/threading at all, and the STW-monitor-race historical attribution can be
retired for real.

---

# 2026-07-13 current-dev residual update

**Status: OPEN.** This remains a real CratonVM-only non-progress failure, but
the current evidence does **not** establish the historical GC/STW-monitor-race
attribution as its root cause. Keep the cross-reference as historical context;
do not use it to close or otherwise classify this current residual.

Validated on `origin/dev` at `acf9aaf7`, in isolated worktree
`/data/victor-worktrees/cratonvm-es-ivfknn-complete-20260712-1540`, using
binary
`/data/data/cratonvm-targets/es-ivfknn-complete-20260712-1540/release/cratonvm-es-ivfknn-complete-20260712-1540`
and the existing Elasticsearch fixture
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`.
The current `others.tsv` selection is runner start `2538` (not the historical
start `549`):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/victor-worktrees/cratonvm-es-ivfknn-complete-20260712-1540/apps/elasticsearch-suite-runner/.suite-es-ivfknn-complete-20260712-1540" -Exe /data/data/cratonvm-targets/es-ivfknn-complete-20260712-1540/release/cratonvm-es-ivfknn-complete-20260712-1540 -JdkHome /home/victor/jdk25 -TimeoutSec 90 -RunName ivfknn-current -ModeName craton-current -Start 2538 -Count 1
```

Results:

- The same selection passes on HotSpot/JDK 25 in 13.5 seconds.
- With Craton JIT enabled, the runner aborts after roughly 5--8 seconds with
  `Test abandoned because suite timeout was reached`; the in-test message says
  `>=580000 msec`. This is not the outer 90-second runner timeout. Narrow
  `System.nanoTime` and `TimeUnit.MILLISECONDS.toNanos(580000)` probes return
  correct values, so a generic timeout-clock or `TimeUnit` conversion fault is
  not an adequate explanation.
- With `--nojit`, the process consumes about one CPU continuously and fails to
  produce a test result before a 90-second outer timeout. A 150-second run
  behaved the same. This is genuine non-progress, not merely a slow test.

A Craton watchdog dump during the no-JIT run places the active worker in the
vector-query path:

```
testSlicesSparseWithFilter -> doTestSlicesSparse -> doTestSlices
-> IndexSearcher.search/rewrite -> IVFKnnFloatVectorQuery.rewrite
-> AbstractIVFKnnVectorQuery.rewrite -> TaskExecutor.invokeAll
-> searchLeaf -> IVFKnnFloatSlicedVectorQuery.getLeafResults
-> CodecReader.getSortedDocValues -> Lucene90DocValuesProducer.getSorted
-> IndexInput.randomAccessSlice -> MockIndexInputWrapper.slice
-> ByteBuffersIndexInput.slice
```

The suite coordinator is waiting in `ThreadLeakControl.join` while that worker
does not advance. A local, uncommitted experiment which decoded raw compact
`long` arguments for `ByteBuffersDataInput.seek(long)` and `slice(long,long)`
also still timed out in no-JIT mode at 90 seconds; it was deliberately not
merged because it did not fix the residual.

The host disallows non-parent `gdb -p` attachment through Yama ptrace policy,
so native thread-state confirmation remains unavailable without a permitted
parent/debug launch. The next investigation should obtain that capture (or
equivalent VM instrumentation) around the `TaskExecutor`/`ByteBuffersIndexInput`
path, rather than treating the older GC-monitor finding as proven for this
current run.

---
# ES HANG - server org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests

Status: OPEN

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard4`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.126`
- tests parsed: `0`
- failed parsed: `0`
- note: ``

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun`
- Branch used for collection: `codex/es-nonpassed-rerun-20260708-191002`
- Collection binary: `/data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002`
- Binary base dev SHA: `3d61003bbfdf9c6b045d29afefd45519dc558881`
- Docs generated after isolated worktree fast-forwarded to dev SHA: `8736a20b6e269bae3ec89d44e22117e2d4eba9a0`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-3ff8aa1c4b -ModeName repro-3ff8aa1c4b -Start 549 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/logs/server.org.elasticsearch.search.vectors.DiversifyingChildrenIVFK.5df450c3f4cb.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/logs/server.org.elasticsearch.search.vectors.DiversifyingChildrenIVFK.5df450c3f4cb.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/results.tsv`

Extracted stderr signals:
- `2026-07-09T05:10:44.709577Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:10:44.709598Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `==== jstack at approximately timeout time ====`
- `2026-07-09T05:16:18.150553Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:16:18.150573Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:16:18.150578Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
- `2026-07-09T05:16:18.150582Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computati...`
Current classification:
- 600 second class watchdog timeout in the completed four-shard collection run.
- Treat as an open hang until reproduced or disproved on current `dev`.


---

## 2026-07-10 investigation (fix/es-vectors-ivfknn-hang-20260710)

**Status: OPEN** (unchanged) — the underlying interpreter-level deadlock this
doc describes is real and NOT fixed. What changed: a JIT regression that had
started masking the hang behind a much-faster SIGSEGV is now fixed, so the
suite runner will show the original 600s HANG again (not a crash) until the
deadlock itself is root-caused.

### Regression found and fixed: guarded-inline-getfield SIGSEGV

Reproducing on current `dev` (tip `f4ee4065` at investigation time, worktree
`/data/data/wt-es-vectors-ivfknn-hang-20260710`, binary
`cratonvm-es-vectors-ivfknn-hang-20260710`) with the doc's own repro command
(`-Dtests.method=testSlicesDense`, same seed) no longer hangs for 600s under
JIT — it **SIGSEGVs almost immediately** (~1-2s in):

```
dmesg: SUITE-Diversify[97470]: segfault at 4c ip 0000760e80332071 sp ... error 4
```

gdb (`handle SIGUSR1/SIGUSR2 nostop noprint pass` first, per
[[reference_linux_gdb_signal_passthrough]]) shows the fault inside JIT-compiled
code, an array bounds-check/load sequence (`mov 0xc(%rax),%r10d` = array
length read) with `rax=0x40` — a small int-shaped value, not a real object
pointer, being dereferenced as one; `0x40 + 0xc = 0x4c` matches the dmesg
fault address exactly. The corrupted value traces back to a local variable
slot fed by a preceding `getfield`.

**Bisection:**
- `CRATONVM_JIT_GETFIELD_HELPER=1` (forces the checked-helper-only getfield
  path) makes the SIGSEGV disappear — the run reverts to the ORIGINAL
  interpreter hang this doc describes (confirms the SIGSEGV is a NEW
  regression sitting on top of the pre-existing hang, not a different bug).
- `CRATONVM_OSR_NEWARRAY=0` (the sibling `perf/throughput-20260710` change)
  does NOT avoid the crash — rules out OSR-newarray re-enable as the cause.
- Root cause isolated to commit `07dfa5e0` ("Guard inline JIT getfield with
  heap-region bounds check (default on)", merged to dev via `756c4b84`,
  see [[project_perf_throughput_20260710]]). Static review of the guard's
  region-containment codegen and the per-object `GC_FLAG_COMPACT` routing did
  not surface a specific wrong-instruction bug with full confidence — this is
  hot JIT x64 codegen and a wrong guess risks a worse regression, so rather
  than patch the codegen blind, `guarded_inline_getfield_enabled()`
  (`jit/src/x64.rs`) was flipped from default-ON to opt-in
  (`CRATONVM_JIT_GUARDED_GETFIELD=1`), matching this codebase's own established
  pattern for an unproven fast path. This is a real, measurable performance
  give-back (bt18 was 7080ms/8.2x with the guard on, vs ~13s helper-only per
  the original commit's own A/B) until someone re-derives the exact
  corrupting instruction and re-enables it default-on.
- Fixed on branch `fix/es-vectors-ivfknn-hang-20260710`, JIT test suite
  (970 tests: 888 lib + 82 `ir_vs_singlepass` differential) green after the
  flip (one new test and one whole differential-test file —
  `ir_vs_singlepass.rs`, purpose-built around the guarded-inline path — needed
  updating to explicitly opt in via the same env var, since they'd relied on
  the old default-on behavior).

### Underlying interpreter hang — now characterized in detail, still OPEN

Reproduced directly (bypassing the suite runner) with
`CRATONVM_JIT_GETFIELD_HELPER=1` (or `--nojit`) and
`--stack-dump-on-timeout=90`. The process genuinely spins (82-112% CPU, not
blocked/idle) with **zero forward progress** across repeated watchdog dumps
during Lucene's `IndexWriter` flush/merge machinery for `testSlicesDense`.
Exact contention point **varies run to run** (seed is fixed but real-thread
scheduling isn't) — seen stuck in, across different runs:
- `org/apache/lucene/util/FileDeleter.decRef`/`getRefCountInternal`
  (confirmed via `javap` these are NOT `synchronized` in Lucene 10.4's
  `FileDeleter`, so this is not a lock — the interpreter frame table's `pc`
  simply stops advancing here across the 3s grace period)
- `IndexFileDeleter.logInfo` (also not synchronized)
- A live `Lucene Merge Thread` blocked entering
  `MockDirectoryWrapper.maybeThrowDeterministicException()` — confirmed
  `synchronized` via `javap` on the real `lucene-test-framework` jar — while
  another `Lucene Merge Thread` (`IndexWriter.mergeMiddle`) sits in a
  legitimate `Object.wait()` (wait-site frame captured via
  `CRATONVM_DBG_MONENTER=1`)

A live gdb attach (`gdb -p <pid>`, no signal needed since it's spinning, not
crashed) during one run caught a genuine two-thread situation: the worker
thread and a `Lucene Merge Thread` each blocked in
`vm/src/threading/monitor.rs::Monitor::enter` (`monitor_enter_synchronized_method`,
`vm/src/vm/vm_exec.rs:1104`) at the same moment, on different Lucene
`synchronized` methods.

**Ruled out:**
- `Monitor::enter`/`wait`/`notify` (`vm/src/threading/monitor.rs`) were
  read in full — the reentrant-owner fast path and the wait/notify condvar
  loops are already hardened with explicit "LOST-WAKEUP FIX" handling for the
  interrupt-races-notify case. No obvious bug found by inspection.
- A GC/monitor-registry desync matching the historical
  [[reference_bugv_nonmoving_sweep_monitor_remap]] (BUG-V) shape was
  considered (this workload allocates heavily) — a differential test with
  `--Xmx 8g` (much less GC pressure) still hung, weakening but not
  conclusively ruling this out (8g can still GC under this workload's
  allocation volume).
- The `gen_heap.rs`/`g1.rs` OOB-field-read WARN bursts this doc originally
  flagged (`class_id=ClassId(0) num_slots=0 java/lang/Object`) fire in a tight
  ~1.6ms burst and then stop — they are NOT the hang itself (the thread keeps
  running for tens of seconds afterward before getting stuck elsewhere); per
  the guard's own log message these are believed-benign speculative
  collection-layout probes, not corruption.

**Not yet found:** the actual mechanism that leaves a thread spinning forever
with no forward progress. Given the contention point moves between runs, this
smells like a genuine timing-dependent VM-core bug (lock-order or a
notify/wakeup gap under specific interleavings) rather than one fixed bad
instruction — needs dedicated concurrency-debugging time, ideally with
`RUST_BACKTRACE`+`CARGO_PROFILE_RELEASE_DEBUG=2` symbols and a tighter, more
deterministic repro (e.g. forcing single-threaded merging) than this full ES
suite test.

**Repro recipe** (direct invocation, bypasses the suite runner's own
possibly-broken jstack — see below):
```bash
ES=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch
CP=$(tr -d '\r' < "$ES/server/build/craton-testcp.txt" | tr '\n' ':' | sed 's/:$//')
CRATONVM_ENABLE_NATIVE_RING=1 CRATONVM_DBG_MONENTER=1 "$EXE" \
  --java-home /home/victor/jdk25 --stack-dump-on-timeout 90 --Xmx 2g \
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home="$ES" -Djava.awt.headless=true \
  -Dtests.method=testSlicesDense \
  <standard ES test JVM args - see run-elasticsearch-suite.ps1 Get-EsJavaArgs> \
  -cp "$CP" org.junit.runner.JUnitCore \
  org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```
Note: `apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1`'s own
`craton-testcp.txt` classpath files have **CRLF line endings** — a naive
`tr '\n' ':'` leaves a trailing `\r` on every path component and breaks
classloading (`Could not find or load main class org.junit.runner.JUnitCore`)
before you even get to the hang; strip with `tr -d '\r'` first.

Also: the suite runner's own `"==== jstack at approximately timeout time ===="`
section (visible in this doc's original evidence) is **empty** — no thread
dump content follows it, on this build. Use CratonVM's own
`--stack-dump-on-timeout=N` watchdog (dumps real interpreter frame chains +
a thread summary) instead of relying on that marker for future repros of this
cluster.

**Verified NOT the same bug as `afa4a6fd`** (String compact-layout
field-offset dual-dispatch, TaskInfoTests SIGSEGV, landed on `dev` while this
investigation was in progress): merged `origin/dev` into this branch and
re-tested with `CRATONVM_JIT_GUARDED_GETFIELD=1` (forcing the guard back on)
— still SIGSEGVs. The two are separate, if superficially similar
(compact/legacy-layout dispatch), bugs in different code paths (generic
getfield inline arms vs. hand-rolled String-intrinsic field loads).


---

## 2026-07-10 follow-up: confirmed as the GC audit's Finding 1 (STW/monitor race)

Deep-dived the underlying interpreter hang with live gdb + a full-debug-info
rebuild (`CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_PROFILE_RELEASE_STRIP=none` —
the normal release profile is line-tables-only and can't resolve locals/args
for a `self`-based inspection). Found this is the SAME bug as
`docs/known-issues/gc-audit-2026-07-10-open-findings.md` finding 1
("STW/monitor race family: GC and monitors vs excluded threads"), an
actively-investigated VM-core defect discovered independently via a
synthetic MTChurn stress harness. Full cross-reference and new evidence
(a genuine Lucene NPE manifestation, not just the deadlock) recorded in
that doc. Summary:

- Direct live-gdb inspection (multiple attempts) confirms the worker thread
  AND a `Lucene Merge Thread` genuinely parked in
  `parking_lot::Condvar::wait` inside `Monitor::block_enter`/`enter`
  (`vm/src/threading/monitor.rs:457/500`) simultaneously — with only 6
  threads total in the process and the other 4 idle/legitimately elsewhere,
  no live thread is positioned to ever release/notify what these two are
  waiting on.
- The manifestation is **non-deterministic run to run, same seed**: most
  runs hang forever; one 153s run instead completed with 2 real failures,
  including `NullPointerException` on `ReadersAndUpdates.dropMergingUpdates()`
  (`rld` null — should never happen) thrown from a Lucene merge thread,
  immediately preceded by a 544+ occurrence burst of the
  `gen_heap::get_field` OOB-field WARNs this doc originally flagged. This
  is the same "corruption escaping as a downstream exception" shape the GC
  audit doc describes for finding 1(b), via a different (real, non-synthetic)
  trigger workload.
- Did NOT attempt a fix. `vm/src/threading/monitor.rs`/the STW barrier is
  already being investigated by a dedicated effort with its own probe
  tooling; a WIP attempt at part of the fix
  (`wip/gc-stw-quota-race-20260710`) is explicitly parked as unsafe
  ("DO NOT MERGE... hangs completely under load... still SEGVs"). This ES
  repro is left as an additional, real-world validation case for whoever
  picks that investigation back up — see the GC audit doc for the repro
  recipe cross-reference and status.

**Status stays OPEN** — root cause is now well-characterized and tied to a
known, tracked, actively-investigated VM-core defect rather than an
ES/Lucene-specific bug. Not expected to be independently fixable without
the GC-audit team's monitor/STW-barrier work landing first.


---

## 2026-07-10 follow-up: guarded-inline-getfield SIGSEGV root-caused and FIXED; flag re-enabled default-ON

The unproven fast path this doc flipped to opt-in has been root-caused, in a different
investigation the same day: the WildFly Host Controller invoke-inline-cache SIGSEGV
(`docs/internal/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md`).
Root cause: the vm-side JIT field resolvers (`vm/src/runtime/interpreter.rs`) fabricated a
`(0, false)` "compact slot" for any field with NO genuine registered `CompactLayout` entry
(`compact_field_slot(...).unwrap_or((0, false))`), and the compact-offset inline getfield arm
(`jit/src/x64.rs`) trusted it — a REFERENCE field with the fabricated `is_ref=false` fell into
the int-category match arm and got a 32-bit `MOVSXD` (sign-extended) load of half a 16-byte
`Value` cell, producing exactly this doc's "receiver `0x40`, a small int value used as an array
pointer" shape (the loaded bytes are the cell's uninitialized tag/payload boundary padding —
non-null, 8-aligned, and totally bogus). Fixed: the resolver now returns `Option<(u32, bool)>`
for the compact slot (never fabricates), and `'L'`/`'['` type tags get an explicit 64-bit
payload load in the compact arm as defense-in-depth.

Re-verified clean on branch `fix/ivfknn-guarded-getfield-reverify-20260710`: ran this doc's
exact repro (`testSlicesDense`, same seed) with `CRATONVM_JIT_GUARDED_GETFIELD=1` — **no
SIGSEGV, no dmesg segfault entry**. Both this run and a baseline run (flag unset) hit the
IDENTICAL pre-existing watchdog-timeout abort (the STW/monitor-race hang described below,
unaffected by this flag) at the 90s `--stack-dump-on-timeout` mark — confirming the SIGSEGV is
gone and the only remaining blocker for this class is the already-tracked, separate hang.
`guarded_inline_getfield_enabled()` is re-enabled default-ON (opt out with
`CRATONVM_JIT_GETFIELD_HELPER=1`); `cargo test --release -p cratonvm-jit` — `--lib` 889/889,
`ir_vs_singlepass` 82/82 (matching this doc's own prior baseline), all other integration test
binaries green, except two PRE-EXISTING, unrelated SIGSEGVs in `intrinsic_string_access`/
`intrinsic_string_search` (confirmed independent of this flag — crash identically with
`CRATONVM_JIT_GETFIELD_HELPER=1` forcing the old default too; spun off as a separate follow-up,
not investigated further here).

---

## 2026-07-11 addendum: independent JIT-cache invalidation gap found (unrelated to this SIGSEGV's actual cause)

While independently re-investigating this cluster (in parallel with, and
without visibility into, the `fix/ivfknn-guarded-getfield-reverify-20260710`
work documented immediately above), found and fixed a real but **separate**
defect: `ClassManager::upgrade_synthetic_class` / `recompute_subclass_layouts`
(`classloading/src/class_manager.rs`) can change a class's field layout
mid-run (synthetic JDK stub -> real `.class` bytecode found on the
classpath), shifting the byte offsets already-JIT-compiled `getfield`/
`putfield` code may have baked in as x86 immediates — and nothing evicted
that stale-compiled code. `install_jit_invalidate_hook` already existed
(already called from `redefine_class`'s Step 8) but had **zero installers
anywhere in the VM**, so that call was always a silent no-op.

This is NOT the cause of this doc's SIGSEGV — that is conclusively the
fabricated-`(0, false)`-compact-slot bug described in the follow-up section
above, verified against this doc's own repro with a clean run (no SIGSEGV,
no dmesg entry). This is a different, independently-real gap surfaced while
looking at the same code paths: nothing else in the VM evicted JIT code
after a layout-changing synthetic-stub upgrade, so a similar "stale baked
offset" corruption could occur through that mechanism even with the
fabricated-slot bug fixed. Fixed by wiring up `install_jit_invalidate_hook`
in `vm/src/vm/vm_init.rs` (mirroring the existing
`resolution_invalidate_adapter` pattern) and calling it from both
layout-changing paths. Regression test added in
`classloading/src/class_manager.rs`
(`recompute_subclass_layouts_fires_jit_invalidate_hook_for_changed_descendants`,
verified to fail without the fix). Full `cratonvm-classloading` (549 tests),
`cratonvm-jit --lib` (889 tests), and targeted `cratonvm-vm` redefine tests
pass with the fix.
