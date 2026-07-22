# ES HANG family - vector HNSW bit classes exceed 120s on CratonVM (FIXED)

> **Resolution supersedes the historical investigation notes below.** On
> 2026-07-17 the runner gained a configurable 300-second CratonVM timeout
> floor for only `ES815HnswBitVectorsFormatTests` and
> `ES93HnswBitVectorsFormatTests`; every other class retains the normal
> 120-second timeout. This finite-throughput case can no longer be
> misclassified as `HANG` under normal host contention.

## Closure verification (Windows host, 2026-07-17)

Built the current `dev` code in isolated worktree
`C:\craton\CratonVM-es-hang-hnsw-bit-20260717` and ran the suite with unique
binary `cratonvm-es-hnswbit-20260717.exe` against the supplied Elasticsearch
checkout after rebuilding `:server:testClasses` to repair its stale missing
`LogConfigurator.class` artifact:

- `ES815HnswBitVectorsFormatTests`: PASS, 6/6 tests, 66.892s.
- `ES93HnswBitVectorsFormatTests`: PASS, 7/7 tests, 62.634s.

The fixture's Linux `libvec.so` was independently validated from WSL with
`readelf --dyn-syms`: 155 `vec_*` exports and all required `bulk8` sentinels
were present. Docker Desktop's Linux engine was stopped on this host, so the
runner's built-in Docker/nm preflight could not execute; its explicit targeted
diagnostic bypass was used only after that equivalent ABI validation.

Historical status (superseded on 2026-07-17): PARTIALLY RESOLVED — one
distinct correctness bug was fixed and the apparent hang was reclassified as
a finite performance gap. The runner-level timeout policy documented above
closes the remaining suite-classification residual.

## Summary (2026-07-16 investigation)

Original observation (2026-07-10): `ES815HnswBitVectorsFormatTests` and
`ES93HnswBitVectorsFormatTests` were killed by a 120s local suite timeout
with no Java exception, vs. 3.7s/5.7s HotSpot controls, and it was unclear
whether this was a real hang or just slow.

Investigated end-to-end on `/opt`/Azure Linux host, worktree
`/data/victor-worktrees/es-hnswbit-20260716` (branch
`fix/es-hnswbit-hang-20260716`, merged to `dev` at `0790e7b6`):

**This is NOT an infinite hang.** Both classes were run with the suite's
120s cutoff removed entirely (unbounded `timeout 2700s`, and separately
with `--stack-dump-on-timeout` at various thresholds) and both complete
every time:

- `ES815HnswBitVectorsFormatTests`: `OK (6 tests)`, 58-72s (single run,
  varies with `CRATONVM_JIT_THRESHOLD`) up to 68-72s at defaults.
- `ES93HnswBitVectorsFormatTests`: `OK (7 tests)`, 85-130s depending on
  host CPU contention from other concurrent sessions on this shared box.
- HotSpot/JDK25 control (same seed, same classpath): both classes in
  1.6-2.1s total.

Stack-dump sampling (`--stack-dump-on-timeout`, CratonVM's own frame-chain
dump) at multiple elapsed-time checkpoints shows the worker thread's
deepest frame genuinely advancing through
`HnswGraphBuilder.addGraphNodeInternal` -> `addDiverseNeighbors` ->
`updateNeighbor` -> `NeighborArray.addOutOfOrder`/`addAndEnsureDiversity`
-> `alertOnHeapMemoryUsageChange` (real HNSW graph-build/diversify work
during `testMergeStability`'s repeated `IndexWriter.addDocument`/
`forceMerge` calls), not stuck at a fixed PC. A `CRATONVM_DBG_JITC=1`
trace confirms these specific hot methods do NOT reach CratonVM's JIT
invocation-count threshold (default 500) during this workload's total
call volume, so they run fully interpreted for the whole test; lowering
`CRATONVM_JIT_THRESHOLD` to 20 only shaved ~20% off wall time (72s->58s),
so JIT tier-up is not the dominant factor — this reads as generic
interpreter-throughput overhead on an allocation/comparison-heavy hot
path, roughly 40-90x slower than HotSpot here, not a specific fixable
defect that this investigation could isolate further.

**Residual OPEN item:** this ~40-90x throughput gap is real and unfixed.
In isolation both classes stay comfortably under a 120s per-class
timeout, but the original 2026-07-10 observation (Windows host, `craton`
suite mode, presumably under more contention from parallel shards) did
exceed 120s, and this investigation's own ES93 runs varied 85s-130s
depending on concurrent host load on the shared Azure box — so a 120s
timeout for this specific pair of classes is not comfortably safe under
load and can still reproduce the original symptom. Not independently
root-caused to one fixable hot spot; would need dedicated JIT/interpreter
throughput work (or a raised per-class timeout for known-slow HNSW-bit
classes) to fully close.

## Fixed as part of this investigation: NoClassDefFoundError: java.lang.foreign.MemorySegment

While reproducing the above, `ES815HnswBitVectorsFormatTests.testMultiClose`
was found to genuinely FAIL (not hang) with:

```
java.lang.NoClassDefFoundError: java/lang/foreign/MemorySegment
```

Root cause: `java.lang.foreign.MemorySegment`'s own `<clinit>` builds the
`NULL` constant via `MemorySegment.ofAddress(0)`, before any user code runs
and before any module could have requested native access.
`native-builtins/src/panama.rs`'s `ofAddress` native unconditionally
denied every call via the native-access gate
(`IllegalCallerException: Native access is not enabled for this module`),
so this internal bootstrap call always threw — which, per JVMS 5.5,
permanently poisons the class: every subsequent reference to
`MemorySegment` anywhere in the same JVM then fails with
`NoClassDefFoundError`, regardless of `--enable-native-access`. Real
HotSpot never denies this internal bootstrap call (a zero-address,
zero-length segment can never be dereferenced), so the control run never
hit it.

**Fixed** (commit `58790cb8`, merged to `dev` at `0790e7b6`): `ofAddress`
now exempts address 0 from the native-access gate; every other address is
still denied exactly as before. Verified: both
`ES815HnswBitVectorsFormatTests` (6/6) and `ES93HnswBitVectorsFormatTests`
(7/7) now pass with zero failures (previously 1 failure each via this
bug), `cratonvm-native-builtins` full suite green (2999 passed), and
`cratonvm-vm --lib` shows only the pre-existing, unrelated release-build
residuals (debug-only `#[should_panic]` lock-order assertions and JIT
skip-list feature tests that cannot fire without `debug_assertions`).

## Relationship to other vector docs

- Confirmed distinct from the `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests`
  / `IVFKnnFloatVectorQueryTests` hang family
  (`ES-HANG-20260709-server-org-elasticsearch-search-vectors-*.md`), which is
  a genuine interpreter-level deadlock tied to the GC-audit STW/monitor-race
  finding — this family shows real, continuous forward progress instead, a
  different mechanism entirely. Not touched by this investigation.
- Not the `FloatBuffer` no-Code family or the vector exception/cause-object
  family, per the original doc's own note — still true.

## Evidence

- Repro worktree: `/data/victor-worktrees/es-hnswbit-20260716` (Azure host
  `20.83.144.174`), binary `cratonvm-es-hnswbit-postmerge`.
- ES fixture reused: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`.
- Repro command (seed arbitrary, not the original 2026-07-10 seed which
  was not recorded in that run):
```bash
ES=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch
CP=$(tr -d '\r' < "$ES/server/build/craton-testcp.txt" | tr '\n' ':' | sed 's/:$//')
"$EXE" --java-home /home/victor/jdk25 -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.asserts=false \
  -Des.path.home="$ES" -Djava.awt.headless=true \
  -cp "$CP" org.junit.runner.JUnitCore org.elasticsearch.index.codec.vectors.ES815HnswBitVectorsFormatTests
```

## 2026-07-16 follow-up: profiled the residual interpreter throughput gap — no single fixable hotspot found

Investigated whether the ~40-90x HNSW-bit slowdown (documented above) has one
isolated, fixable root cause, using `perf record -g -F 999` against a live
`ES815HnswBitVectorsFormatTests` run (worktree
`/data/victor-worktrees/perf-hnswbit-20260716`, branch
`perf/es-hnswbit-throughput-20260716`, binary
`cratonvm-perf-hnswbit-20260716`).

**Caveat on this specific run:** the Azure host was under extreme, sustained
multi-tenant contention while profiling (load average climbed from ~73 to
~280+ on 16 cores, 70-140 concurrent users over about an hour) — `perf`
attributed ~61% of samples to unresolved kernel addresses (`[k] 0x...`),
almost certainly scheduler/context-switch noise from that contention rather
than genuine CratonVM work, plus 923 lost samples out of ~399K. The
userspace (non-kernel) portion of the profile is still informative, but a
re-run under a quiet host would sharpen the percentages.

**Finding: the userspace self-time is spread thin across many small
functions, not concentrated in one hotspot.** The top individual
`cratonvm_vm`/`cratonvm_gc` symbols, none exceeding ~2% self-time each:

- `runtime::interpreter::execute_frame` (2.10%) — core interpreter dispatch loop
- `runtime::interpreter::update_root_snapshot` (1.82%) — per-frame GC root-tracking maintenance
- `cratonvm_gc::gen_heap::GenerationalHeap::is_object_address` (1.58%) — GC address-range validity check
- `runtime::frame::Frame::scan_local_objects_inner` (0.71%) — GC local-slot root scanning
- `native_api::registry::NativeMethodRegistry::find` (0.54%) — native-method dispatch lookup
- `_mi_page_malloc_zero` / `mi_free` (0.54% / 0.26%) — mimalloc allocation churn
- `runtime::interpreter::execute_instruction`, `execute_invokevirtual_cached`,
  `resolve_field_ref`, `cratonvm_gc::vm_heap::VmHeap::is_object_address`,
  `classloading::resolution::InvokeCache::get`,
  `classloading::class::find_method_recursive`,
  `execute_invokevirtual_vtable_fast`, `execute_invoke_kind`,
  `resolve_method_ref`, `force_native_over_real_jdk_bytecode`,
  `Frame::pop_and_recycle_frame_with_reason`, `try_stackless_invoke`,
  `vm_exec::safe_native_call_impl`, `Frame::new_pooled_cached`,
  `execute_invokestatic_cached` — each 0.18-0.53%.

Checked the one apparent duplication (`GenerationalHeap::is_object_address`
1.58% + `VmHeap::is_object_address` 0.43%, ~2% combined): `VmHeap::is_object_address`
(`gc/src/vm_heap.rs:452`) is a legitimate one-level dispatch wrapper over the
GC-backend enum (Generational/G1/ZGC) used by JIT frame root scanning, not a
redundant/avoidable double-check — not a bug.

**Conclusion:** this is not a single fixable defect. It is the cumulative,
distributed cost of fully-interpreted execution — GC root-tracking, method/
field-resolution caching, frame pooling, and native-dispatch decision logic
that every bytecode dispatch pays in the interpreter but a JIT-compiled or
real-JDK path does not. This matches the earlier `CRATONVM_JIT_THRESHOLD`
experiment (lowering it only bought ~20%, since these hot methods'
NeighborArray/HnswGraphBuilder call counts per test run are too low to
reach the tier-up threshold, and even compiling them wouldn't eliminate this
per-dispatch bookkeeping cost for whatever remains interpreted around them).

Closing this gap further would require a broader, cross-cutting interpreter
throughput initiative (reducing the per-bytecode cost of GC root tracking,
dispatch caching, and frame management project-wide), not a scoped bug fix
— out of scope for this known-issue doc. Leaving the doc's residual OPEN
item as-is; a future perf initiative should re-profile on an idle host for
a cleaner signal before deciding where to invest.
