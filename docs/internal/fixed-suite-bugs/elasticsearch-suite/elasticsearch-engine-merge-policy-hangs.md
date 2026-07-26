# Elasticsearch engine merge-policy hangs

Status: RETIRED 2026-07-08 (fixed on current dev; residual folded into the young-GC weak-reference survivorship fix)

## 2026-07-08 retirement

This tracker is retired out of `../../../known-issues` because both pieces it carried are now closed or superseded by fixed root-cause trackers:

- the original deterministic A5/root-snapshot throughput bug was fixed by `cbae5cd0` (`scan_active_jit_frames` no longer rescans the already-verified native-stack suffix on every object-returning helper);
- the later 2026-07-05 corrupt-header/RRWL residual matched the young-GC weak-reference survivorship bug fixed by `0c6cbd58`, with the follow-up crawl-regime fix in `2072699b`. The concrete runtime changes are that `weakref_null_referents_pre_gc` now watches Reference object addresses as well as referents, and `GenerationalHeap::is_live_young_survivor` lets reference processing recognize kept-in-place young survivors after the non-moving sweep.

Fresh Azure validation from isolated branch `codex/fix-es-engine-merge-policy-residuals-20260708` used the unique binary `/data/data/target-es-engine-residuals-20260708/release/cratonvm-es-engine-residuals-baseline-20260708`:

- `RwlReadTearingProbe 8 8000` completed 4/4 with every reader and the writer joined; this is the focused RRWL/weak-reference probe for the residual mechanism that made this note stay open after the A5 scan fix.
- A direct `ShuffleForcedMergePolicyTests` rerun was not possible on that host: no compiled Elasticsearch checkout/classpath was present (`craton-testcp.txt` and the test class were absent under `/data/data`). If that class is rebuilt and again exceeds the 300s suite timeout on current `dev`, file a new known-issue note with fresh logs rather than reopening this stale combined tracker.


Date observed: 2026-07-02
Date investigated: 2026-07-03/04
Date partial-fix landed: 2026-07-04

## Summary

`ShuffleForcedMergePolicyTests` was reported as a 300s suite-timeout "HANG"
under CratonVM JIT-on. Deep investigation (symbolicated native-stack capture
+ CPU profiling + targeted instrumentation) shows this is **not a deadlock**:
left running, the process completes on its own — it is simply catastrophically
slower than HotSpot for this specific test (HotSpot: 15s).

One genuine, safe performance bug was found and fixed (below), giving a real
but partial speedup. The test still exceeds the 300s suite timeout, so it
still needs to be treated as a hang/hangs-class defect by the suite runner
until further work lands.

## Root cause of the slowdown

The test exercises Lucene's `NamedSPILoader`, which reflectively constructs
dozens of backward-compat `Codec`/`PostingsFormat`/`DocValuesFormat`
providers via `ServiceLoader`. Each provider construction triggers a deep,
legitimate chain of nested class-initialization (`ServiceLoader` iteration →
reflective `Constructor.newInstance` → `AccessController.doPrivileged` →
`MethodHandle`/`MethodHandles.Lookup.findStatic` dispatch → further nested
`<clinit>`s), observed via a symbolicated (`profsym`) `cdb` capture as ~130
native stack frames deep / ~30-40 interpreter frames deep.

`update_root_snapshot` runs on every object-returning native call (confirmed:
**~1.6-2.8 million calls** in under 90 seconds of this test). Its frozen-frame
cache (`rootsnap_cache`) and cross-GC cache survival
(`rootsnap_cache_survive_gc`) are both working correctly and were **not** the
bottleneck (confirmed via targeted instrumentation: near-zero GC cycles
during the slow phase, near-perfect frame-cache hit rate).

The actual dominant, *provably growing* cost was the "unregistered JIT frame"
safety-net scan in `scan_active_jit_frames`
(`../../../../vm/src/jit/conservative_roots.rs`, the A5-fix block): on **every** call
(whenever any method has ever been JIT-compiled — i.e. essentially always),
it scanned `[search_lo, stack_high)` for a stray JIT return address, where
`search_lo` tracks the current native stack pointer. Since that pointer only
gets deeper as this workload's nested-reflection call chain grows, the
scanned range — and therefore the cost of every single snapshot — grew
monotonically over the run (measured: ~3µs → ~15µs per call across 2.4M
calls, accounting for the large majority of `update_root_snapshot`'s own
cost, which itself was roughly half of total wall-clock time).

## Fix landed

`../../../../vm/src/jit/conservative_roots.rs`: memoize the deepest stack pointer already
verified clean of an unregistered JIT frame, keyed additionally on
`jit_code_range_count()` (invalidated by any new compilation, closing the one
theoretical staleness gap — an OSR/late-registration race). Once a region
`[verified_lo, stack_high)` scans clean, any later call whose `search_lo >=
verified_lo` is checking a subset of an already-verified-clean range (nothing
above the current stack pointer can change while this thread is nested below
it), so the scan is skipped. New compilations invalidate the memo and force a
fresh scan, so the safety net (Windows-only "A5" unregistered-`main`-frame
detection) is preserved exactly as before.

**Verified:**
- Same test outcome before/after (both hit the pre-existing, separately
  tracked `RandomizedContext.getPerThread()` NPE — see below — not a new
  regression).
- Instrumented (`CRATONVM_DBG_ROOTSNAP=1`) A/B: the per-call cost driven by
  this scan dropped from a growing ~3-15µs/call to a much smaller, far more
  slowly growing ~0.1-2.6µs/call.
- Full-run timing: JUnit-reported internal time dropped from 2,277s to
  1,416s for this test (~1.6x) — a real, substantial improvement, but the
  test **still exceeds the 300s suite timeout**.

## Residual gap

Even after the fix, this test takes ~1,416s (vs HotSpot's 15s) before hitting
the pre-existing `RandomizedContext.getPerThread()` NPE documented in
[elasticsearch-randomizedcontext-per-thread-null.md](elasticsearch-randomizedcontext-per-thread-null.md)
(a genuine JIT-timing-dependent race, already root-caused there — not
re-investigated here). Closing the remaining gap to HotSpot parity (or at
least under the 300s timeout) needs further profiling of the *other* costs in
this reflection/classloading-heavy path (dozens of SPI providers, each paying
real `ServiceLoader` + reflection + nested-`<clinit>` overhead); no further
single dominant bottleneck was identified in this pass.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1569 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-engine-merge-policy-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

To reproduce and observe the (fixed) `update_root_snapshot` cost directly,
run the class standalone (see `../../../CONFIG.md` for CLI flags) with
`CRATONVM_DBG_ROOTSNAP=1` and watch the periodic `[ROOTSNAP]` stderr lines.

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.engine.ShuffleForcedMergePolicyTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

Fix branch: `fix/es-engine-merge-policy-hangs-20260703` (worktree
`C:\craton\CratonVM-es-mergehang-20260703`), commit touches
`../../../../vm/src/jit/conservative_roots.rs` only.

The overlapping HotSpot-fail class mentioned in the original report,
`org.elasticsearch.index.engine.InternalEngineTests`, was not investigated
here (separate defect).

## Deterministic closure (2026-07-04)

The deterministic A5 safety-net bottleneck that was slowing this class is
confirmed fixed on current `dev` via `cbae5cd0` (`fix/es-engine-merge-policy-hangs-20260703`):
the `scan_active_jit_frames` O(depth) growth path is now memoized and no
longer regresses into `O(depth)` for this test profile. The remaining
runtime gap is only the separate `RandomizedContext.getPerThread()` residual
currently documented in
[`elasticsearch-randomizedcontext-per-thread-null.md`](elasticsearch-randomizedcontext-per-thread-null.md).

## 2026-07-05 recheck after RandomizedContext policy fix

This note is still open. The earlier "Deterministic closure" section only closed the A5 unregistered-JIT-frame safety-net bottleneck; it did not close the suite-level hang.

Rechecked `org.elasticsearch.index.engine.ShuffleForcedMergePolicyTests` directly from current `origin/dev` (`0833a4766`) using the uniquely named binary:

```text
/data/target-es-engine-closure-20260705/release/cratonvm-es-engine-closure-azure-20260705
```

The run was killed by a 360-second outer timeout before any JUnit completion line, so it still exceeds the suite runner's 300-second hang threshold. The log advanced through Elasticsearch/Lucene bootstrap and then remained in the slow test body; this is not fixed by the already-retired RandomizedContext issue.

New diagnostic signal near timeout:

```text
GC: inconsistent header - kind=Object but array_length=512 (num_slots=0, class_id=0); inline-alloc forgot to set kind=Array. Treating as corrupt so the walker can re-sync.
mark_young: rejecting object ... implausible extent ... corrupt header, not marked/scanned
```

This mirrors the corrupt-header warning previously noted during the binary-doc-values/AQS investigation and suggests a remaining JIT/GC allocation-header corruption or stale-reference path can surface in this reflection/classloading-heavy engine test. It is not yet isolated enough for a code fix in this pass.
