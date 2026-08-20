# HTTP/2 cluster: the two flow-controller classes are FIXED by passing `-ea`; `DataCompressionHttp2Test` is CONFIRMED to be the snappy wall and nothing new

**Status: RETIRED 2026-08-19 as a page.** Both open questions the predecessor
left are answered, and the one actionable fix it named has landed and been
validated across the whole 733-class suite.

Superseded page: `known-issues/netty/http2-flowcontroller-and-datacompression-triage-20260819.md`.

| | predecessor said | now |
|---|---|---|
| `UniformStreamByteDistributorFlowControllerTest` | harness gap, "worth doing once" | **FIXED** — `-ea` added to `common.args`, 28/6 → 34/0 |
| `WeightedFairQueueRemoteFlowControllerTest` | same | **FIXED** — same, 28/6 → 34/0 |
| `DataCompressionHttp2Test` | "**Not confirmed** … worth a longer-`await()` experiment" | **CONFIRMED** the known snappy throughput wall; the experiment was run |

---

## 1. The two flow controllers — the harness never passed `-ea`, and now does

The predecessor page had this right and stopped one step short of the fix. Both
classes extend `DefaultHttp2RemoteFlowControllerTest` and fail the identical 6
inherited methods, all of which assert that netty's own internal `assert`
statements throw — no-ops unless the JVM is launched with `-ea`.

The VM half landed a week earlier
(`ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md`: the launcher
discarded the flag, plus three latent `java.lang.invoke` shim defects that only
became reachable once it stopped being discarded). Re-verified on today's `dev`,
isolated, one class per process:

| class | no `-ea` | with `-ea` |
|---|---|---|
| `UniformStreamByteDistributorFlowControllerTest` | `found=34 ok=28 failed=6` | **`found=34 ok=34 failed=0`** |
| `WeightedFairQueueRemoteFlowControllerTest` | `found=34 ok=28 failed=6` | **`found=34 ok=34 failed=0`** |

So the VM was ready and only the harness was not. `apps/netty-suite-runner/common.args`
now carries `-ea` as its last line. (`apps/` is gitignored and lives on the
shared build host, which is why the predecessor page declined to change it and
recorded the reason instead; the change is one line and the pre-change file is
kept beside it as `common.args.pre-ea-20260819`.)

### Why a full-suite A/B was needed before touching a shared file

`-ea` is not scoped. It switches on every `assert` in every class the run
touches, netty's and the JDK's, so "does it fix these two" is the small half of
the question. The whole 733-class list was run twice on one binary
(`cvm-nettytri-base`, dev `c6c81ec5b`), 6 shards, 180 s cap, arms back to back:

| | ABORTED | FAIL | HANG | NOTESTS | PASS |
|---|---:|---:|---:|---:|---:|
| no `-ea` | 22 | 93 | 37 | 40 | 541 |
| with `-ea` | 21 | 91 | 41 | 40 | 540 |

36 classes changed status between the arms — far more than `-ea` can explain,
and in both directions on classes with no `assert` in sight
(`HttpResponseEncoderTest` HANG→PASS, `NioDatagramChannelTest` PASS→HANG). That
is what a 180 s cap does on a build host shared with other agents, and it is
exactly the trap `a flaky test reverted a good fix` was written about. Every one
of the 36 was therefore re-run **isolated, one class per process, three times per
arm**.

Thirteen of them (both targets, every candidate regression, and two of the eight
`io.netty.buffer` classes that went PASS→HANG as a cluster — the shape that
would show up if `-ea` had slowed a 400-test class past the cap), `ok/failed/aborted`
per rep:

| class | no `-ea` ×3 | with `-ea` ×3 |
|---|---|---|
| **`UniformStreamByteDistributorFlowControllerTest`** | 28/6/0 28/6/0 28/6/0 | **34/0/0 34/0/0 34/0/0** |
| **`WeightedFairQueueRemoteFlowControllerTest`** | 28/6/0 28/6/0 28/6/0 | **34/0/0 34/0/0 34/0/0** |
| `AdvancedLeakAwareByteBufTest` | 426/0/0 ×3 | 426/0/0 ×3 |
| `BigEndianHeapByteBufTest` | 414/0/0 ×3 | 414/0/0 ×3 |
| `CertificateBuilderTest` | 39/28/7 ×3 | 39/28/7 ×3 |
| `ChannelInitializerTest` | 9/0/0 ×3 | 9/0/0 ×3 |
| `HttpContentCompressorTest` | 26/0/0 ×3 | 26/0/0 ×3 |
| `LocalChannelTest` | 26/0/0 ×3 | 26/0/0 ×3 |
| `MultiThreadIoEventLoopGroupTest` | 2/0/0 ×3 | 2/0/0 ×3 |
| `SimpleLeakAwareByteBufTest` | 425/0/0 ×3 | 425/0/0 ×3 |
| `SimpleLeakAwareCompositeByteBufTest` | 497/0/9 ×3 | 497/0/9 ×3 |
| `SingleThreadEventLoopTest` | 18/0/0 ×3 | 17/1/0 18/0/0 18/0/0 |
| `HashedWheelTimerTest` | 14/0/0 13/1/0 13/1/0 | 13/1/0 ×3 |

**Two classes flip and eleven do not.** The two that flip are the targets, and
they flip deterministically, 3 for 3. The two rows with a mixed cell flake in
*both* arms — `SingleThreadEventLoopTest` once in six runs, `HashedWheelTimerTest`
in five of six — so neither is attributable to the flag; the buffer cluster is
identical in both arms and `-ea` is not even slower on it (28.9 s vs 34.3 s on
`AdvancedLeakAwareByteBufTest`, the faster run being the `-ea` one).

That is the evidence for touching a file every other agent on the build host
shares. The pre-change file is kept as `common.args.pre-ea-20260819` and the
whole rationale is appended to the runner's `NOTES.md`, so the next reader does
not have to find this page to know why the flag is there.

---

## 2. `DataCompressionHttp2Test` — confirmed, and confirmed to be old news

The predecessor page inferred that this was the already-documented snappy
throughput wall and said plainly that it had not measured it. It has been
measured now, three ways, and the inference holds.

### 2.1 It is CratonVM-specific and it is exactly two tests

`probes/PerTestProgressRunner.java` (per-method `@@BEGIN`/`@@END`, so a budget
overrun names which test spent it), isolated, host load 1.2:

| | class total | `encodingTooBigMessage` snappy #4 / #8 | every other test |
|---|---:|---|---|
| HotSpot 25 | 1 613 ms | 99 ms / 28 ms, both SUCCESSFUL | 15–99 ms |
| CratonVM | 18 482 ms | **6 409 ms / 6 430 ms, both FAILED** | 83–133 ms |

Both failures are `assertTrue(serverLatch.await(5, SECONDS))` at
`DataCompressionHttp2Test.java:255`. Note what the sibling rows say: the *same
test method* at its gzip/deflate/br/zstd parameterisations costs 119–130 ms on
CratonVM. This is not a class-wide slowdown; it is one codec.

### 2.2 The longer-`await()` experiment the predecessor asked for

The class was recompiled from netty's own source with `await(5, SECONDS)` →
`await(600, SECONDS)` (two sites) into an override directory placed ahead of
`test-classes` on the classpath, changing nothing else:

```
SUCCESSFUL 10956ms  encodingTooBigMessage(int, AsciiString)   <- snappy
SUCCESSFUL  8143ms  encodingTooBigMessage(int, AsciiString)   <- snappy
SUCCESSFUL   211ms  encodingTooBigMessage(int, AsciiString)
SUCCESSFUL   198ms  encodingTooBigMessage(int, AsciiString)
...  all 42 SUCCESSFUL
```

**All 42 pass given time.** There is no wrong answer, no lost frame, no hang —
the server side finishes, in 8.1 s and 11.0 s against a 5 s budget. The failure
is a wall-clock budget and nothing else, which is the thing the predecessor page
could not assert.

### 2.3 It is the same mechanism, not a coincidentally similar one

`--dump-native-registry` over the (passing, 600 s) run — 18 371 699 native calls,
top rows:

| invocations | native |
|---:|---|
| **5 356 015** | `java/lang/invoke/VarHandle.get([Ljava/lang/Object;)Ljava/lang/Object;` |
| 2 099 620 | `java/nio/ByteBuffer.limit()I` |
| 2 099 455 | `jdk/internal/util/Preconditions.checkFromIndexSize(…)I` |
| 2 098 178 | `java/lang/ref/Reference.reachabilityFence(…)V` |
| 2 098 114 | `java/nio/DirectByteBuffer.session()…` |
| 1 049 566 | `java/nio/ByteBuffer.position()I` |
| 1 049 123 | `java/nio/DirectByteBuffer.isReadOnly()Z` |
| 1 049 088 | `jdk/internal/misc/ScopedMemoryAccess.copyMemory(…)V` |

`VarHandle.get` at 29 % of all native calls is netty 4.2's reference-count check
— `AbstractByteBuf`'s checked accessors call `ensureAccessible()` → `refCnt()`,
and 4.2 reads that field through a `VarHandle`. That is the *identical*
signature `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` recorded for snappy
(21 368 822 `VarHandle.get` per 4 MiB, ~5.1 per output byte), from a different
netty class, on the same codec. The `ByteBuffer`/`ScopedMemoryAccess` cluster
below it is one bulk copy plus its six-native preamble, in a constant 7:1 ratio.

`perf record` over the same run agrees and adds nothing a counter did not: a
flat profile whose head is the interpreter and the native-registry lookup
(`NativeMethodRegistry::find` 2.80 %, `slot_index_for_key` 0.83 %,
`find_with_kind` 0.80 %, `slot_for_exact` 0.77 %, plus `__memcmp` 2.45 %).

**Disposition:** no new defect. This class is a second member of the snappy
per-call-cost list, with a smaller budget (5 s) than the compression cluster's
180 s cap. It needs ~2× on that path to clear its own `await`. That belongs to
`httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`, which already names the next
measurable step for it.

---

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
CV=/data/cvm-<wt>/cvm-<name>

# 1 — the flow controllers, both arms
$CV --java-home "$JAVA_HOME" --Xmx 1500m      @common.args -Dcraton.batch=1 CratonRunner \
    io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest   # ok=28 failed=6
$CV --java-home "$JAVA_HOME" --Xmx 1500m -ea  @common.args -Dcraton.batch=1 CratonRunner \
    io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest   # ok=34 failed=0

# 2 — DataCompressionHttp2Test, per test, with the budget lifted
sed 's/serverLatch.await(5, SECONDS)/serverLatch.await(600, SECONDS)/g' \
  /data/cratonvm/apps/netty/codec-http2/src/test/java/io/netty/handler/codec/http2/DataCompressionHttp2Test.java \
  > override/src/io/netty/handler/codec/http2/DataCompressionHttp2Test.java
javac -nowarn -cp "$NETTY_CP" -d override/out override/src/io/netty/.../DataCompressionHttp2Test.java
$CV --java-home "$JAVA_HOME" --Xmx 1500m -Djunit.jupiter.execution.timeout.mode=disabled \
   -cp "override/out:probes/out:$NETTY_CP" PerTestProgressRunner \
   io.netty.handler.codec.http2.DataCompressionHttp2Test
```

The override directory has to come FIRST on the classpath; `test-classes` holds
the unmodified copy and the classpath order is the whole mechanism.

## Related

* `known-issues/netty/fail-hang-crash-rerun-20260817.md` — where this cluster was flagged.
* `ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md` — the VM half of the `-ea` fix, and the page that predicted this harness change would be safe once it landed.
* `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md` — owns the snappy residual.
* `compression-testhugedecompress-shared-timeout-20260816.md` — the sibling class list on the same wall.
