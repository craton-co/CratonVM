# ES HANG - server org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard3`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.178`
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

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.InetAddressRandomBinaryD.65f0e9e5676f.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.InetAddressRandomBinaryD.65f0e9e5676f.err.log`

Extracted stderr signals (original collection):
- `==== jstack at approximately timeout time ====`
- `NOTE: reproduce with: gradlew test --tests InetAddressRandomBinaryDocValuesRangeQueryTests.testRandomTiny -Dtests.seed=B17AC9D3E1F2A0C4 ...`
- `WARN [RandomizedRunner] Will linger awaiting termination of 3 leaked thread(s).`

## 2026-07-10 investigation, part 1: the original hang mechanism (FIXED)

Same underlying mechanism as the sibling
[Long](long-random-binary-doc-values-range-query-tests-FIXED.md)/
[Integer](integer-random-binary-doc-values-range-query-tests-FIXED.md)/
[Double](double-random-binary-doc-values-range-query-tests-FIXED.md)
`RandomBinaryDocValuesRangeQueryTests` classes: on a binary built strictly
after the `3d61003b` collection SHA, the 600s hang had turned into a 100%
deterministic JIT SIGSEGV (`ReentrantLock.unlock()`'s `getfield this.sync`
miscompiled to a 32-bit truncating load — see the Long doc for the full
writeup). Fixed upstream (concurrent sessions, commits `7f96c26c` +
`be710234`; not this doc's own investigation). Confirmed on a clean dev-tip
checkout (`e768916a`, no local changes) that this SIGSEGV/hang no longer
occurs for this class.

## 2026-07-10 investigation, part 2: a different, genuine correctness bug

With the SIGSEGV/hang gone, the class now runs to completion but **fails a
real assertion inside the test** — this is a Lucene/`RangeType.IP`-level
correctness bug, unrelated to the getfield/JIT mechanism above. Reproduced
twice on the same clean dev-tip binary, same seed, different runs (test
method ordering/sub-seeds vary run-to-run even at a fixed
`-Dtests.seed`):

Run A (`testRandomTiny`, iter id=233):
```
FAIL (iter 0): id=233 should match but did not
 queryRange=Box(89.179.120.84/89.179.120.84 TO 8247:6e4b::62e1:ce85:ffff:ffff/8247:6e4b:0:0:62e1:ce85:ffff:ffff)
 box=Box(::/0.0.0.0 TO d147:bc96:ffff:ffff:da22:2d9a:ffff:ffff/0.0.0.0)
 queryType=CONTAINS
 deleted?=false
```

Run B (`testRandomMedium`, iter id=385):
```
java.lang.AssertionError: wrong hit (first of possibly more):
FAIL (iter 0): id=385 should match but did not
 queryRange=Box(89.179.120.84/89.179.120.84 TO 8247:6e4b::62e1:ce85:ffff:ffff/8247:6e4b:0:0:62e1:ce85:ffff:ffff)
 box=Box(42.42.42.42/42.42.42.42 TO ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/0.0.0.0)
 queryType=CONTAINS
 deleted?=false
```

Both failures are a `CONTAINS` query mismatch where the query range's min is
an IPv4 address and max is a "real" (non-IPv4-mapped) IPv6 address, against a
stored box whose max prints with a trailing `/0.0.0.0`.

## 2026-07-11 investigation, part 3: root cause and fix (FIXED)

The `/0.0.0.0` in every failing box print was the tell: it's the exact
fallback default `native-builtins/src/net_phase_e.rs` uses when it cannot
resolve a synthetic `InetAddress` mirror's real address. Root-caused to
**two stacked defects** in that file's `InetAddress` mirror machinery, both
inside `inet_addr_resolve` — the fallback reader `getHostAddress()` /
`getAddress()` / `toString()` consult whenever a mirror isn't found in the
process-global `inet_addr_side_table()`:

1. **Missing GC root registration (primary cause).**
   `inet_addr_side_table()` — a `HashMap<ObjectRef, (hostName, ipAddress)>`
   — was never registered as a GC root/remap target: no `gc_scan_*` /
   `gc_update_*` pair existed for it in `vm/src/memory/roots.rs` /
   `vm/src/memory/gc.rs`. Same bug class as `BUG-U`, the stale-Locale GC
   root (`docs/internal/CRATONVM_BUGS/BUG-U-stale-locale-gc-root-sigsegv.md`).
   A moving young GC that relocates a live `InetAddress` mirror leaves the
   table keyed on a vacated from-space slot; the next lookup misses and
   falls through to the (also broken, see below) fallback.
2. **Broken IPv6 fallback (secondary, always-on defect).** The fallback
   path only read the base `InetAddress$InetAddressHolder.address` int
   field, which is always `0` for an `Inet6Address` — the real 16 bytes
   live in a *separate* `Inet6Address$Inet6AddressHolder.ipaddress` field
   (`holder6`). Any `Inet6Address` reaching this fallback — a side-table
   miss from (1), **or** any address built via the un-overridden
   `InetAddress.getByAddress(String, byte[])` two-arg factory (which never
   populates the side table at all, since only the one-arg overload is
   natively overridden) — deterministically reported `"0.0.0.0"` instead of
   its real value, with zero GC involved.

With thousands of allocations per random test iteration, (1) fires
probabilistically once a GC actually relocates an already-side-tabled
mirror — explaining why the *query range* (built fresh, just before use)
usually prints correctly while the *stored box* (built earlier, more likely
to have survived an intervening GC) is the one that goes stale.

Fix (`native-builtins/src/net_phase_e.rs`, `vm/src/memory/roots.rs`,
`vm/src/memory/gc.rs`):
- Added `gc_scan_inet_addr_roots` / `gc_update_inet_addr_refs`, mirroring
  the existing Locale / re10-HttpHandler root-scan pattern in the same
  file, and wired the pair into `roots.rs` / `gc.rs`.
- `inet_addr_resolve`'s fallback now checks `holder6.ipaddress` (formatted
  through the existing `hotspot_ip_string` canonicalizer) before falling
  back to the IPv4-only `holder.address` int.

Verified on the Azure host (`victor@20.83.144.174`):
- A standalone probe
  ([`InetGcProbe.java`](../../gcprobes/InetGcProbe.java): construct 16-byte
  IPv6 `InetAddress`es via both `getByAddress(byte[])` and
  `getByAddress(String, byte[])`, force `System.gc()` churn, re-read
  `getAddress()` / `getHostAddress()`) reproduces both defects on a
  pre-fix binary — GC churn corrupts a previously-good 16-byte address
  down to a 4-byte `0.0.0.0`; the two-arg factory returns `0.0.0.0`
  immediately, no GC needed — and matches real-JDK-25 output byte-for-byte
  on the post-fix binary in every case.
- `InetAddressRandomBinaryDocValuesRangeQueryTests` itself
  (`-Dtests.seed=B17AC9D3E1F2A0C4`, `--Xmx 2g`): **6 of 7 pre-fix runs**
  hit this doc's exact `CONTAINS` / `/0.0.0.0` failure signature; **0 of 9
  post-fix runs** did (8/9 clean `OK (6 tests)`, one hit an unrelated
  `java/util/Set` GC-staleness NPE — a different code path, not this bug).
  The remaining 1/7 pre-fix run hit a different, unrelated
  `ClassCastException` with no stack trace. Neither stray failure
  reproduces this doc's signature. Both are pre-existing, already-tracked
  defects, unrelated to the fix here: the `Set` NPE is a suspected
  unpinned-local GC hazard in `native-collections`'s `Set.of(...)` builder;
  the `ClassCastException` is a third independent real-world corroboration
  of the deep, already-tracked monitor-vs-evacuation race — see
  [`gc-audit-2026-07-10-open-findings.md`](../../gc-audit-2026-07-10-open-findings.md#second-cross-confirmation-from-an-independent-real-world-trigger-2026-07-11)
  finding 1(b), "Second cross-confirmation" section (a follow-up
  investigation of this same doc's residual reproduced it 3/20 runs and
  confirmed a tight correlation with an `IllegalMonitorStateException`
  precursor). Neither blocks this doc's retirement.

Re-run:
```
org.junit.runner.JUnitCore org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests
# -Dtests.seed=B17AC9D3E1F2A0C4, --Xmx 2g, against a built `server` module classpath.
```
