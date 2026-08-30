# The 38 compatible-mode FAILs, triaged — 17 fixed, 9 not CratonVM's, and one shared cause under a third of the rest

## Result

| bucket | n | fixable? |
|---|---:|---|
| **FIXED here** — `NativeImageHandlerMetadataTest` x17 | **17** | done, harness only |
| Load flake: passes quiet on CratonVM | 5 | nothing to fix in the VM |
| HotSpot fails identically | 3 | not CratonVM's |
| Classpath order in the fixture | 1 | documented already |
| Real, already documented | 3 | open, has pages |
| **`testMutualAuthSameCertChain`, one cause across 4 SSL classes** | 4 | **open, quantified below** |
| `JdkSslEngineTest`, a different SSL failure | 1 | open, not yet characterised |
| `testHugeDecompress` throughput wall | 4 | open, has pages |

Sixteen of the 38 rows never needed a VM change, and finding that out cost less
than investigating any one of them would have.

## The 17 that are fixed, and why they were never a defect

Every `NativeImageHandlerMetadataTest` compares the `ChannelHandler` subtypes it
can reach against **its own module's** checked-in `reflect-config.json`. On the
flat whole-reactor classpath it reaches every module's handlers, so it cannot
pass. The runner already models this — `module-scoped-classes.tsv` gives each of
the 17 a module directory and argfile — but the machinery was inert on this
host for two reasons:

* `run-netty-suite.sh` hard-codes `USE_MODSCOPE=0 # module scope disabled by
  default on Windows`, not overridable by env, and
* `gen-module-args.sh` had Linux-only defaults: `mvn` on `PATH` (Windows has
  `mvnw`), `/data/...` paths, and `:` as the classpath separator.

Both fixed on this host — `MVN`/`CPSEP`/`USE_MODSCOPE` are now env-overridable,
so the Linux host's behaviour is byte-for-byte unchanged — and then:

```
run-netty-suite.sh --list modscope-classes.txt  (USE_MODSCOPE=1)
status: PASS=17   wall 16s
```

**17 of 17, in 16 seconds**, where the same classes had been 17 red rows
costing 17 s to 51 s each. `not-cratonvm-bugs-consolidated.md` has carried them
since 2026-08-19 as a permanent environment gap; they are not one.

This is the third time on this fixture that a red cluster was one line of
harness configuration — after the missing `-ea` and the missing
`-Dio.netty.bootstrap.extensions=serviceload`.

## The 5 that pass on a quiet host

The categorize run puts six classes on the box at once, so its FAIL column is
not a quiet-host verdict. Re-run one class at a time:

| class | HotSpot | CratonVM | failing test |
|---|---|---|---|
| `NioEventLoopTest` | 13/13 | **13/13** | `testSelectableChannel()` timed out after 3000 ms |
| `SslHandlerTest` | 53/53 | **53/53** | `testSessionTicketsWithTLSv1{2,3}()` after 5000 ms |
| `JdkZlibIntegrationTest` | 11/11 | **11/11** | `testHugeDecompress()` after 120 s |
| `AutoScalingEventExecutorChooserFactoryTest` | — | 18/18 targeted | see its own page |
| `AbstractReferenceCountedTest` | 3/3 | 3/3 | — |

**Do not generalise from one member of a cluster.** `JdkZlibIntegrationTest`
passes quiet and the other four compression classes do NOT — they still fail
`testHugeDecompress` at 132-159 s against the test's own 120 s cap. One arm of
five looked like a cluster-wide flake and was not.

## The 3 where HotSpot fails the same way

| class | HotSpot | CratonVM |
|---|---|---|
| `NioUdtByteRendezvousChannelTest` | `ok=1 failed=1` | `ok=1 failed=1` |
| `ResourceLeakDetectorTest` | `ok=2 failed=1` | `ok=2 failed=1` |
| `CertificateBuilderTest` | identical result set, both directions (existing page) | same |

`NioUdtByteRendezvousChannelTest.basicEcho()` fails on both with a byte-count
mismatch (`expected: <1188976> but was: <1141280>`) and belongs on the
consolidated not-a-bug page. `ResourceLeakDetectorTest` already has a CLOSED
page saying it is slow, not hung.

## The one shared cause worth fixing: `testMutualAuthSameCertChain`

Four SSL classes fail it, and it is the same test each time:

| class | occurrences |
|---|---:|
| `OpenSslJdkSslEngineInteroptTest` | 23 of its 25 |
| `JdkOpenSslEngineInteroptTest` | 3 of 3 |
| `ReferenceCountedOpenSslEngineTest` | 3 of 4 |
| `OpenSslEngineTest` | 1 of 1 |

Run that method ALONE, 48 parameterisations, quiet, one VM each:

| | result | wall |
|---|---|---:|
| HotSpot 25 | **48/48** | ~~92 s~~ **23 s** |
| CratonVM | ~~did not finish~~ **48/48** | ~~>900 s~~ **452 s** |

**CORRECTED 2026-08-30** — the struck numbers were taken while 6 `cargo` and 8
`rustc` from other sessions were on this box. Re-measured quiet the pair is
23 s and 452 s, i.e. **~20x**, and CratonVM **completes the method**: it does
not fail on a quiet host at all. 452 s over 48 parameterisations is 9.4 s each
against the test's own 30 s `@Timeout` — about 3x of headroom, which six-way
contention erases. So this is a throughput gap that presents as a timeout under
load, not a hard failure. The claim that this one method sits under 30 of the
38 rows' individual failures stands. See
`mutualauth-certchain-throughput-20260830.md`.

That is one method, one cause, and 30 of the 38 rows' individual failures. It
is the highest-value open item in the netty suite and it does not have a page
yet — this is it, pending a root cause.

## Left as-is, with reasons

* `DataCompressionHttp2Test` — 2 snappy failures, quiet, HotSpot clean. Already
  confirmed as the known snappy throughput wall in
  `internal/fixed-suite-bugs/netty/http2-flowcontroller-ea-and-datacompression-snappy-20260819.md`,
  which ran the longer-`await()` experiment. Re-confirmed here, nothing new.
* `HashedWheelTimerTest`, `RecyclerTest` — both have pages from this week.
* `BouncyCastleEngineAlpnTest` — the fixture's `-cp` puts `bcprov-jdk15on-1.70`
  ahead of `1.84`; existing consolidated-page row.
* `JdkSslEngineTest` — 2 x `expected: <false> but was: <true>`, not the
  `testMutualAuthSameCertChain` shape and not characterised here.

## Harness changes this needed

All three are on the untracked Windows fixture, so they exist on this host
only and are written down here because that is the only durable place:

| file | change |
|---|---|
| `run-netty-suite.sh` | `USE_MODSCOPE` env-overridable (was hard `0`); `CV_EXTRA_FLAGS` for launcher flags with no `CRATONVM_*` spelling, echoed in the mode header |
| `gen-module-args.sh` | `MVN` and `CPSEP` overrides so it runs with `mvnw` and `;` on Windows |
| `module-args/` | generated, 17 argfiles |

Every default is unchanged, so the Linux host behaves exactly as before.

## Reproduce

```bash
cd apps/netty-suite-runner
# the 17
MVN=../netty/mvnw CPSEP=';' NETTY_SRC=C:/craton/CratonVM/apps/netty \
  MAVEN_REPO_LOCAL=C:/Users/<you>/.m2/repository bash gen-module-args.sh
USE_MODSCOPE=1 bash run-netty-suite.sh --list modscope-classes.txt

# the shared SSL cause
<vm> --java-home <jdk25> @common.args -Dcraton.batch=1 MethodRunner \
 'io.netty.handler.ssl.OpenSslJdkSslEngineInteroptTest#testMutualAuthSameCertChain(io.netty.handler.ssl.SSLEngineTest$SSLEngineTestParam)'
```

## Related

* `full-suite-refresh-20260829.md` — the run these 38 come from.
* `full-suite-jdk-only-20260830.md` — the same 657 under `--jdk-only`.
