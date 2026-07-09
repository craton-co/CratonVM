# ES CRASH family - current-dev fail-probe representatives exit 139

Status: RESOLVED (crash family) — see 2026-07-09 verification update below.
The `rc=139` crash family described here is fixed; the doc is kept for
history and because the ES suite is still far from green (new blockers
found during verification, see below).

Update (2026-07-09):
- The rc=139 crash surface is now known to be triggered by Panama synthetic layout mismatches in two fallback paths:
  - `java/lang/foreign/Linker.defaultLookup()` was allocating a 0-field `SymbolLookup`.
  - `java/lang/foreign/Linker.downcallHandle(..., [Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;` was allocating a short `MethodHandle` shape.
- The crash-family code path is now aligned in `native-builtins/src/phases_late.rs` to return a 2-field `SymbolLookup` (with `-1` default lib marker) and a 4-field `java/lang/foreign/DowncallHandle` carrying `fn_addr`, descriptor, and variadic metadata.
- No full non-passed rerun has been completed since this change, so `Status` remains OPEN until the `es-nonpassed-currentdev` family is re-run and 139s drop.

Source:
- Probe run: `es-faildocs-probe-20260709-073704`
- Trigger: representative rerun of old FAIL rows after the large `findNative` and `SymbolLookup.find` families were fixed on `dev`.
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- Mode: CratonVM JIT on.
- Hang timeout: 600 seconds.

Exact count:
- Selected representatives: 14.
- CratonVM JIT PASS: 1.
- CratonVM JIT FAIL: 2.
- CratonVM JIT CRASH: 11, all rc=139.

Crash rows:
- `libs/cli-terminal org.elasticsearch.cli.terminal.JsonTerminalTests` -> rc=139, note includes `MemoryLayout.varHandle` AbstractMethodError.
- `server org.elasticsearch.index.codec.postings.ES812PostingsFormatTests` -> rc=139, stderr shows out-of-bounds field reads on `java/lang/foreign/SymbolLookup` and `java/lang/invoke/MethodHandle` before exit.
- `server org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v1DiskBBQVectorsFormatTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.index.codec.zstd.Zstd814BestCompressionStoredFieldsFormatTests` -> rc=139, stdout includes `MemoryLayout.varHandle` AbstractMethodError.
- `server org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQBFloat16VectorsFormatTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBFloat16VectorsFormatTests` -> rc=139, no Java-level exception captured.
- `client/rest org.elasticsearch.client.RestClientSingleHostIntegTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.lucene.queries.FloatRandomBinaryDocValuesRangeQueryTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswScalarQuantizedBFloat16VectorsFormatTests` -> rc=139, stderr logs `updateDocument(Term, Iterable)J` NoSuchMethodError before exit.
- `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBinaryQuantizedBFloat16VectorsFormatTests` -> rc=139, no Java-level exception captured.
- `server org.elasticsearch.search.vectors.IVFKnnFloatSlicedVectorQueryTests` -> rc=139, no Java-level exception captured.

Baselines:
- HotSpot passed 13 of the same 14 representatives. The only HotSpot FAIL was the Zstd fixture-native-library row, not an rc=139 crash.
- CratonVM --nojit converted many of the same classes into Java-level FAIL or 600s HANG rows, which are documented separately.

Interpretation:
- This is not a full-suite crash count; it is a current-dev residual probe count from old FAIL rows.
- The rc=139 behavior often hides the Java-level residual that is visible under `--nojit`, so use this doc to track the JIT/runtime crash surface and use the fail-family docs for cleaner root-cause signals.
- The `ES812PostingsFormatTests` stderr guard warnings suggest at least one crash path still touches foreign API method-handle/SymbolLookup layout handling.

## Full non-passed rerun update

- Run: `es-nonpassed-currentdev-20260709-082115`
- Binary: `/data/data/cratonvm-targets/es-rerun-currentdev-20260709-082115/release/cratonvm-es-rerun-currentdev-20260709-082115`
- Class list: 2649 non-passed rows from the prior ES selection.
- Timeout: 120 seconds.
- Shards: 4.
- Result: 2640 CRASH, 2 FAIL, 7 PASS, 0 HANG.
- All crash rows exited rc=139.
- 2583 crash result notes directly contain `MemoryLayout.varHandle`; 2585 crash logs contain the same marker.
- 50 crash rows had blank result notes, including 8 with CratonVM GC guard out-of-bounds field markers in captured logs.

Old-HANG rerun update:
- Run: `es-hung10-currentdev-20260709-082115`
- Timeout: 1500 seconds.
- Result: 10 CRASH, 0 HANG.
- All ten old-HANG classes now exit rc=139 before the long timeout matters.

## 2026-07-09 verification update — rc=139 crash family RESOLVED

The `es-nonpassed-currentdev-20260709-082115` run above started at
08:21:15, **before** the `MemoryLayout.varHandle` fix in commit `9494a0a5`
landed on `dev` (09:17:23 the same day) — its 2640 rc=139 crash count is
stale.

Rebuilding from current `dev` and re-verifying surfaced two more bugs the
varHandle crash had been masking end-to-end (both now fixed, see
`docs/internal/fixed-suite-bugs/enummap-realmode-corruption-and-stackwalker-frame-order-FIXED.md`):

1. `EnumMap.<init>` corrupting real-JDK objects (every `EnumMap.put()`
   threw `ClassCastException`, hit via
   `com.carrotsearch.randomizedtesting.Threads.<clinit>` on every JUnit run).
2. `StackWalker.walk()`/`forEach()` handing callers stack frames in
   reversed order, breaking Lucene's `TestSecrets.ensureCaller()` caller
   check (`UnsupportedOperationException: Lucene TestSecrets can only be
   used by the test-framework.`), hit by ~97% of a 99-class worst-offender
   probe once bug 1 was fixed.

With both fixed, a full rerun of the exact same 2649-class `others.tsv`
selection (run `es-fullrerun-fixed-20260709-211207`, same JIT-on / 120s
timeout / 4-shard config, binary built from `dev` merge `528fde12`):

- **0** `rc=139` crashes (down from 2640).
- 2646 FAIL, 2 HANG, 2648/2649 rows collected (1 class,
  `server org.elasticsearch.persistent.PersistentTasksClusterServiceTests`,
  is missing from the aggregated results — likely a concurrent-write drop
  from running 4 shards against one shared `results.tsv`; not
  investigated, rerun it standalone before trusting a future full count).
- The 2 HANG rows are `client/rest RestClientGzipCompressionTests` and
  `client/rest RestClientSingleHostIntegTests` — both **PASSED** in the
  stale 08:21:15 baseline, so this looks like a newly-exposed (not newly
  introduced) timing/network bug, same "next layer" pattern as the two
  bugs above. Not investigated further this session.
- The FAIL rows are now dominated by a **different, already-tracked**
  bug: `EnumSet.allOf`/`EnumSet.of` returns a broken object (`size()==0`,
  `iterator()==null`, `toString()` falls through to `Object@hash`) for
  non-JDK enums, hit via `org.apache.logging.log4j.Level.<clinit>` at
  logging bootstrap in nearly every class. See
  `docs/known-issues/enumset-of-broken-for-non-jdk-enums.md` (already open
  before this session, root-caused by a concurrent session against a
  narrower Tomcat repro) — this session's full-suite rerun confirms it is
  now the dominant ES-suite blocker and adds a new symptom (`toString()`
  falling through to `Object.toString()`, not just empty size/null
  iterator) to that doc.

No full non-passed rerun was re-selected against the *current* `dev` HEAD
(others.tsv reused the original 2649-class list) — once `EnumSet` is
fixed, a fresh `-RefreshLists` run against current `dev` is worth doing
since the PASS/FAIL boundary has likely shifted again.
