# 2026-07-16: FIXED — root cause was a `GAP_FILLER_CLASS_ID` young-GC exact-walk regression (same-day sibling commit), not the GC-audit finding 1(a)/1(b) STW/monitor race this doc previously tracked

**Status: FIXED.** `testRandomWithFilter` (this doc's hang target) now
passes cleanly and reliably. See the full root-cause and verification
write-up in the sibling
[DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests doc](../../known-issues/elasticsearch-suite/ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)'s
2026-07-16 section (same underlying bug, same fix, both classes share the
Lucene `IndexWriter`/`TaskExecutor` code path this doc's own 2026-07-10
sections already identified as the shared trigger).

Summary: a same-day sibling commit (`1c4aaa06`, "close stream ArrayList
pressure corruption") added two new "exact young-object walk" loops to
`../../../../gc/src/gen_heap.rs` that didn't special-case the `GAP_FILLER_CLASS_ID`
TLAB-tail sentinel — misparsing it aborted the exact-walk early and left
every object allocated afterward invisible to the young-gen mark phase,
so the non-moving sweep reclaimed live objects as garbage. This explains
the mass "stale pointer / all-zero header" corruption bursts and the
resulting deadlock/timeout signature this doc originally attributed (2026-
07-10 sections below) to the GC-audit's finding 1 (STW/monitor race) — that
attribution is **superseded**: this cluster's actual proximate cause was a
much simpler, freshly-introduced regression, not the long-standing
monitor/evacuation race (which remains separately tracked, unaffected by
this fix, for the `InetAddressRandomBinaryDocValuesRangeQueryTests`
manifestation — see `docs/internal/gc-audit-2026-07-10-open-findings.md`
finding 1(b)).

Fixed on branch `fix/es-ivfknn-hang-20260716`, worktree
`/data/wt/wt-es-ivfknn-20260716` (Azure host). Verification (4 consecutive
runs, direct invocation, this doc's own seed `B17AC9D3E1F2A0C4`):
`testRandomWithFilter` `OK (1 test)` in ~20-22s each, zero corruption
warnings (was: deterministic hang/timeout before the fix). Also re-verified
this doc's own previously-flagged-but-never-investigated failures
(2026-07-10 section below): `testScoreEuclidean`, `testScoreCosine`, and
`testSkewedIndex` all now pass individually and as part of the full class.

**One new, unrelated finding while re-verifying the full class** (not a
regression from this fix, not present in the single-method runs this doc's
own recipe targets): `testMergeAwayAllValues` fails deterministically via a
`posix_madvise` `EINVAL` through the Panama FFI native-downcall path.
Confirmed absent on real HotSpot (same seed, same classpath) — a genuine,
separate CratonVM bug in a different subsystem. Filed as its own doc:
[`ES-FAIL-20260716-testMergeAwayAllValues-posix-madvise-einval.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260716-testMergeAwayAllValues-posix-madvise-einval.md).

Moving this doc to `../..` since its own named subject
(`testRandomWithFilter`) and its own previously-flagged residual failures
are now all fixed/resolved.

---

# ES HANG - server org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests

Status: OPEN

## 2026-07-13 cross-reference

See the sibling
[DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests doc](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)'s
"2026-07-13 follow-up: array-constructor-reference lambda bug found + fixed"
section — a real, independent, now-fixed interpreter bug
(`SomeType[]::new` array-constructor-reference lambdas allocated a corrupted
object instead of a real array) that reproduces via real Lucene's
`ByteBuffersDataInput` constructor, shared by both classes in this cluster.
Not yet confirmed to be this doc's own hang's root cause — re-run this
class's repro against a build including that fix first.

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard4`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.029`
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
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-565afb965e -ModeName repro-565afb965e -Start 556 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/logs/server.org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/logs/server.org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard4/results.tsv`

Extracted stderr signals:
- `==== jstack at approximately timeout time ====`
Current classification:
- 600 second class watchdog timeout in the completed four-shard collection run.
- Treat as an open hang until reproduced or disproved on current `dev`.


---

## 2026-07-10 investigation (fix/es-vectors-ivfknn-hang-20260710)

**Status: OPEN** (unchanged) — same underlying interpreter-level deadlock
family as
[the DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests doc](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)
(same class of Lucene `IndexWriter`/`MockDirectoryWrapper` `synchronized`-method
contention during a flush/merge, seen across both test classes) — see that
doc for the full investigation writeup, thread-dump evidence, and repro
recipe. Summary for this class specifically:

- A NEW JIT regression (guarded-inline-getfield SIGSEGV, root-caused to
  commit `07dfa5e0`) had started masking this hang behind a much faster
  crash. Fixed on branch `fix/es-vectors-ivfknn-hang-20260710` by flipping
  `guarded_inline_getfield_enabled()` (`../../../../jit/src/x64.rs`) from default-ON to
  opt-in (`CRATONVM_JIT_GUARDED_GETFIELD=1`) — the exact corrupting
  instruction was not pinned down with full confidence via static review, so
  rather than patch hot JIT codegen on a guess, the unproven fast path was
  made opt-in again, matching this codebase's own established pattern.
- The underlying interpreter hang itself (this doc's original subject,
  `IVFKnnFloatVectorQueryTests.testRandomWithFilter`) is a genuine spinning
  deadlock (82-112% CPU, zero forward progress) in Lucene's IndexWriter
  flush/merge synchronization, NOT fixed — needs dedicated concurrency
  debugging time. This class's suite run also showed 3 separate test
  failures earlier in the same class (`testScoreEuclidean`, `testScoreCosine`,
  `testSkewedIndex` — each produced a `NOTE: reproduce with` line before the
  timeout) that were not investigated in this session; only the hang
  (`testRandomWithFilter`) was in scope.


---

## 2026-07-10 follow-up: confirmed as the GC audit's Finding 1 (STW/monitor race)

Same conclusion as
[the DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests doc](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)
— see that doc and `docs/known-issues/gc-audit-2026-07-10-open-findings.md`
finding 1 for the full writeup. This is a VM-core GC/monitor race
(actively investigated separately, WIP fix parked as unsafe), not an
ES/Lucene-specific bug. Status stays OPEN.


---

## 2026-07-10 follow-up: guarded-inline-getfield SIGSEGV root-caused and FIXED; flag re-enabled default-ON

Same root cause and fix as
[the DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests doc](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)
— see that doc for the full write-up (fabricated `(0, false)` compact-field slots in the JIT
field resolver, fixed via
`docs/internal/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md`).
`guarded_inline_getfield_enabled()` (`../../../../jit/src/x64.rs`) is re-enabled default-ON
(`fix/ivfknn-guarded-getfield-reverify-20260710`); re-verified via the sibling class's exact
repro (both classes share the same Lucene IndexWriter flush/merge code path) — no SIGSEGV, only
the already-tracked STW/monitor-race hang below. This class's own 3 separate test failures
(`testScoreEuclidean`, `testScoreCosine`, `testSkewedIndex`, noted in the prior update) were not
re-investigated here — still open, out of scope for this flag re-verification.

---

## 2026-07-11 addendum: independent JIT-cache invalidation gap (unrelated to this SIGSEGV's actual cause)

Same as [the sibling DiversifyingChildren doc's own 2026-07-11 addendum](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md#2026-07-11-addendum-independent-jit-cache-invalidation-gap-found-unrelated-to-this-sigsegvs-actual-cause):
found and fixed a real but separate JIT-cache invalidation gap
(`install_jit_invalidate_hook` had zero installers anywhere in the VM)
while independently re-investigating this cluster. Not the cause of this
SIGSEGV — that is the fabricated-`(0, false)`-compact-slot bug documented
above, already fixed and re-verified. See the sibling doc for the full
writeup.
