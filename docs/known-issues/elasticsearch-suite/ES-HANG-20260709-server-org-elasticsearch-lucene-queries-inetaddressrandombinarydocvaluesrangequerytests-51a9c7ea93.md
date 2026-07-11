# ES HANG - server org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests

Status: OPEN (redescribed 2026-07-10 — original hang mechanism fixed, but a
separate, genuine correctness bug is now exposed and reproducible)

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

## 2026-07-10 investigation, part 2: a different, genuine correctness bug (OPEN)

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
stored box whose max prints with a trailing `/0.0.0.0` — suggestive of an
IPv4-vs-IPv6 encoding/comparison asymmetry in `RangeType.IP`'s
`dvRangeQuery`/CONTAINS byte-comparison path (the min/max encode to fixed
16-byte InetAddress-point byte arrays; an unsigned-byte-order comparison bug
specifically at the IPv4-mapped boundary would produce exactly this shape of
false negative). Not yet root-caused — needs its own investigation, likely
starting from `RangeType.encodeRanges`/`dvRangeQuery` for `RangeType.IP` and
whatever native/real-bytecode path backs `InetAddress`/byte-array unsigned
comparison in this VM.

Not a hang, not a crash — a bounded, single-assertion JUnit failure (`Tests
run: 6, Failures: 1`) that exits cleanly (`rc=1`). Reproduces on pure current
`dev` (checkout `e768916a`), no local modifications.

Re-run:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir "<workdir>" -Exe <cratonvm-exe> -JdkHome <jdk25> -TimeoutSec 90 -RunName repro-inetaddress-cvr -ModeName repro-inetaddress-cvr -Start 1 -Count 1
```
(or directly via `org.junit.runner.JUnitCore
org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests`
against a built `server` module classpath, seed `B17AC9D3E1F2A0C4`.)
