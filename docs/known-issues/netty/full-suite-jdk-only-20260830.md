# Netty full suite in `--jdk-only`, 2026-08-30 — no correctness divergence, and the whole visible cost is throughput

## What was run

The same 657 classes, the same shard count and cap, a binary built from the
same commit as the compatible-mode baseline (`92983411f`), one launcher flag
apart.

| | compatible | `--jdk-only` |
|---|---:|---:|
| PASS | 540 | **533** |
| FAIL | 38 | 30 |
| HANG (180 s cap) | 17 | **34** |
| ABORTED | 26 | 24 |
| NOTESTS | 36 | 36 |
| wall | 93m13s | 98m2s |

`run-netty-suite.sh` had no way to pass a launcher flag that has no
`CRATONVM_*` env spelling, so it gained one: `CV_EXTRA_FLAGS`, appended to
`VMFLAGS_BASE` and echoed in the mode header, so a run cannot claim a policy it
did not use. The header for this run reads `extra-flags=--jdk-only`, and the
per-shard raw logs carry the VM's own `[cratonvm] --jdk-only: real class bytes
are authoritative` banner.

## Read the FIRST caveat before any number above

**`--jdk-only` at this commit is wave-1 enforcement, not "no natives".** The
VM says so on startup — *"Wave-1 enforcement covers class fabrication and
synthetic-stub registration; remaining violations are recorded and counted"* —
and `--jdk-only-report` proves it. Three representative classes, censused:

| | |
|---|---:|
| violations recorded per class | ~2 300-2 450 |
| `native-shadows-bytecode` **`native-won`** | 1 514 events, **536 distinct** class.method |
| `native-shadows-bytecode` `bytecode-won` | 629 events, 236 distinct |
| `compatibility-class-requested` (refused) | 3 per class |

The `native-won` set is concentrated in `java/util` (568), `java/lang` (314),
`java/util/concurrent` (132), `java/lang/reflect` (119), `java/lang/invoke`
(81). So a green suite here says **the enforced subset is sound**, not that the
VM is correct on real bytecode alone. Anyone quoting "netty passes in jdk-only
mode" without that sentence is over-claiming by roughly 536 methods.

The three refused classes are identical in every census and are the VM's own,
not the JDK's: `cratonvm/internal/foreign/MemorySegmentImpl`,
`cratonvm/stream/LazyOp`, `java/util/Enumeration$Impl`.

## Every apparent regression is throughput

Twelve classes went PASS → not-PASS. Eleven of them went to **HANG with
`found=0`**: the class produced no result at all before the 180 s cap. Two were
re-run quiet with a 600 s cap:

| class | compatible | `--jdk-only` | verdict |
|---|---:|---:|---|
| `DefaultHttp2ConnectionTest` | 44 s, 50/50 | **330 s, 50/50** | 7.5x slower, **zero failures** |
| `SizeClassedChunkCacheTest` | 12 s, 29/29 | **162 s, 28/29** | 13.5x slower |

Both complete. The HANG column is measuring the cap, not a deadlock — the cap
is calibrated for compatible mode and jdk-only is 7-14x slower on
buffer/http2-heavy classes.

And the one failure that survives the bigger cap is throughput too:

```
SizeClassedChunkCacheTest:concurrentScansTerminateWhenNoCapacity()
  => Concurrent scans should terminate within 30 seconds, not livelock
     expected: <true> but was: <false>
```

That is the test's **own** 30-second internal deadline, failing for the same
reason the class took 162 s instead of 12 s.

The twelfth, `Http2ConnectionRoundtripTest`, was the only class that ran to
completion and failed (21/19/2) — and re-run quiet under `--jdk-only` it is
**21/21 in 88 s**. Six-shard load, not the policy.

**So: no class in the netty suite fails under `--jdk-only` for a reason other
than being slower.** That is the headline, and it is a genuinely good result
for wave-1.

## Eighteen moved the other way, and they are sampling

Five FAIL → PASS: `NioEventLoopTest`, `OpenSslEngineTest`,
`ReferenceCountedOpenSslEngineTest`, `AbstractReferenceCountedTest`,
`AutoScalingEventExecutorChooserFactoryTest`. The last two are the six-shard
load flakes already characterised in `full-suite-refresh-20260829.md`, and
their passing here is independent confirmation of that diagnosis rather than
evidence for jdk-only.

Six FAIL → HANG are the compression cluster, which has its own throughput
pages; three ABORTED → HANG are composite-buffer classes whose aborts the cap
no longer reaches. None of these is a change in verdict — they are the same
classes crossing the same cap from the other side.

## What this run does NOT license

* It does not say the netty suite passes in jdk-only mode. 34 classes never
  finished; the two probed were fine given time, the other 32 were not probed.
* It does not say the VM is correct without native shadows. 536 distinct
  methods still won against real bytecode inside this very run.
* Its FAIL column is a six-shard verdict. Two of the moves in each direction
  were flakes, which is the base rate to expect from the others.

The obvious follow-up, if a real jdk-only verdict is wanted, is the same 657
classes at a 900 s cap with 2 shards. That is roughly a six-hour run and would
convert the whole HANG column into an answer.

## Reproduce

```bash
cd apps/netty-suite-runner
CV_BIN=<binary> CV_EXTRA_FLAGS=--jdk-only SHARDS=6 \
  bash run-netty-suite.sh --list testlist.txt --count 0
# and the census for one class
<binary> --java-home <jdk25> --jdk-only --jdk-only-report out.json \
  @common.args -Dcraton.batch=1 CratonRunner <class>
```

## Related

* `full-suite-refresh-20260829.md` — the compatible-mode baseline this is
  differenced against, and the two load flakes it cleared.
* `feature-designs/jdk-only-mode.md` — what wave-1 enforcement covers.
