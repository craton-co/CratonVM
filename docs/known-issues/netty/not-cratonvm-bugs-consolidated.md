# Netty non-passing classes confirmed NOT CratonVM bugs — consolidated reference

**Purpose: stop future sessions re-investigating these.** Every entry below
was directly cross-checked against stock HotSpot 25 on the same host, same
classpath, same harness (`run-netty-suite.sh` / `CratonRunner`) — and
HotSpot fails, aborts, or produces the identical result. None of these
belong in a "CratonVM regression" count. Compiled 2026-08-19 from the
2026-08-19 FAIL/HANG/CRASH rerun (`fail-hang-crash-rerun-20260817.md` and
its split-out cluster docs) plus two carried-forward verdicts from the
2026-08-16 doc sweep. Each entry links to the doc with the full evidence;
this page is the index, not a replacement for them.

> **One entry has been WITHDRAWN (2026-08-20), and the reason applies to any
> future one.** `SslErrorTest` was cleared on "identical `found=0 started=0`" —
> a real cross-check, correctly performed, whose conclusion was wrong.
> **Two VMs running nothing is not two VMs agreeing.** The class generated zero
> parameterisations because `OpenSsl.isAvailable()` was false, which was the
> fixture's CLASSPATH rather than the host (see `gen-openssl-args.sh`); with it
> corrected, HotSpot passes 72 and CratonVM fails 12.
>
> Before adding a row here, check that the class actually RAN. A `found` count
> of 0, or one far below the class's own parameterisation count, disqualifies
> the comparison no matter how identical the two sides look. `CloseNotifyTest`
> below is the benign version of the same thing: it was cleared on a matching
> pair of `aborted` counts, and on the corrected classpath both VMs simply pass.

| class | why it's not a CratonVM bug | evidence | doc |
|---|---|---|---|
| `io.netty.channel.NativeImageHandlerMetadataTest` | Resource path computed from Maven `groupId`/`artifactId` metadata this non-Maven harness never populates — literal `null/null` in the path on both VMs | Direct HotSpot cross-check: identical `AssertionFailedError`, identical `null/null` path | `nativeimagehandlermetadatatest-not-a-cratonvm-bug-20260819.md` |
| `io.netty.handler.NativeImageHandlerMetadataTest` | same | Same failure text observed in the raw log; mechanism shared with the class above, not independently re-run on HotSpot | same |
| `io.netty.handler.codec.NativeImageHandlerMetadataTest` | same | same (log pattern match, not independently re-run) | same |
| `io.netty.handler.codec.dns.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.haproxy.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.http.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.http2.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.memcache.binary.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.mqtt.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.redis.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.sctp.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.smtp.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.socks.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.stomp.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.xml.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.proxy.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.resolver.dns.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.buffer.AlignedPooledByteBufAllocatorTest` | `isDirectMemoryCacheAlignmentSupported()` assumption answers `false` on this host for both VMs | Direct HotSpot cross-check: `found=49 ok=21 aborted=28`, byte-identical to CratonVM | `buffer-alignment-abort-cluster-CLOSED-20260819.md` |
| `io.netty.buffer.AdvancedLeakAwareCompositeByteBufTest` | same assumption family | Direct HotSpot cross-check (2026-08-19): `found=506 ok=497 aborted=9`, byte-identical | same |
| `io.netty.buffer.BigEndianCompositeByteBufTest` | same | Direct HotSpot cross-check: `found=496 ok=487 aborted=9`, byte-identical | same |
| `io.netty.buffer.LittleEndianCompositeByteBufTest` | same | Direct HotSpot cross-check: `found=496 ok=487 aborted=9`, byte-identical | same |
| `io.netty.buffer.PooledByteBufAllocatorTest` | same | Direct HotSpot cross-check: `found=47 ok=45 aborted=2`, byte-identical | same |
| `io.netty.buffer.SimpleLeakAwareCompositeByteBufTest` | same | Direct HotSpot cross-check: `found=506 ok=497 aborted=9`, byte-identical | same |
| `io.netty.buffer.WrappedCompositeByteBufTest` | same | Direct HotSpot cross-check: `found=496 ok=487 aborted=9`, byte-identical | same |
| ~~`io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest`~~ | **RESOLVED 2026-08-19 — row retired, not carried.** The harness gap was fixed rather than accepted: `common.args` passes `-ea` now, and both classes are `found=34 ok=34 failed=0` on CratonVM. They are PASSING classes, not not-a-bug classes. | Deterministic, 3 isolated reruns per arm, after a full-suite `-ea` A/B found no other class affected | `http2-flowcontroller-ea-and-datacompression-snappy-20260819.md` |
| ~~`io.netty.handler.codec.http2.UniformStreamByteDistributorFlowControllerTest`~~ | same — RESOLVED, see the row above | same | same |
| `io.netty.handler.ssl.BouncyCastleEngineAlpnTest` | The fixture's `-cp` carries `bcprov-jdk15on-1.70.jar` AHEAD of `bcprov-jdk18on-1.84.jar`, so `bctls-1.84` resolves `NISTObjectIdentifiers` from 1.70 and dies in `TlsUtils.<clinit>` on the missing `id_ml_dsa_44` | Direct HotSpot cross-check: identical `NoSuchFieldError` from the identical BC frame, `found=1 failed=1` both. **RESOLVED 2026-08-20 on the corrected classpath — both VMs now PASS** (`gen-openssl-args.sh --bc18`, one fork each: HotSpot `ok=1 failed=0 ms=877`, CratonVM `ok=1 failed=0 ms=711`). The 2026-08-19 note said "with the 1.70 jars dropped HotSpot PASSES while CratonVM does not"; that remainder was a real CratonVM defect and is fixed — see the retired SSLContext-SPI page. This row is now ONLY about the fixture's jar order | `foreign-nio-subclass-and-bc-provider-object-FIXED-20260819.md`, `sslcontext-natives-ignore-a-third-party-spi-FIXED-20260820.md` |
| `io.netty.handler.ssl.CloseNotifyTest` | ~~`netty-tcnative`/OpenSSL not available in this environment~~ — it was the CLASSPATH, not the environment | Was: identical `found=4 ok=2 aborted=2` on both, same `Assumption failed: OpenSSL is not available`. **RE-TAKEN 2026-08-20 with `gen-openssl-args.sh`: `found=4 ok=4` on both VMs.** The row is still not-a-CratonVM-bug, but for a different reason — nothing is skipped any more, and everything passes | `hashedwheeltimertest-late-task-firing-RETIRED-20260819.md`, `openssl-key-material-and-engine-residuals-FIXED-20260820.md` §D |
| ~~`io.netty.handler.ssl.SslErrorTest`~~ **— WITHDRAWN 2026-08-20, this IS a CratonVM bug** | The cross-check was `found=0 started=0` on both VMs, and that is not agreement — it is two VMs running nothing. The parameterisation yielded zero cases because `OpenSsl.isAvailable()` was false, which was the CLASSPATH | **RE-TAKEN with `gen-openssl-args.sh`: HotSpot `found=72 ok=72`, CratonVM `found=72 ok=60 failed=12`.** All 12 were `clientProvider = JDK` client-side certificate rejections answering `TLSV1_ALERT_ACCESS_DENIED` where a certificate alert is required. **FIXED 2026-08-21 — 72/72, matching HotSpot** | `ssl-client-sends-access-denied-for-every-trust-rejection-FIXED-20260821.md` |
| `io.netty.pkitesting.CertificateBuilderTest` | Result set matches HotSpot exactly, in both directions | `ok=39 failed=28 aborted=7` on both VMs; failing-test-name sets identical (`comm`-verified) | `certificatebuildertest-fail-status-not-a-regression-20260816.md` |
| `io.netty.handler.ssl.PemEncodedTest` | `1 ok / 2 aborted` is steady-state-correct — two intentional `assumeFalse` skips, not a regression | Isolated on G1: `1 ok / 2 aborted` identical on CratonVM and HotSpot | `pemencodedtest-aborted-status-not-a-regression-20260816.md` |
| `io.netty.bootstrap.BootstrapTest` / `io.netty.bootstrap.ServerBootstrapTest` | ~~a ServiceLoader/classpath artifact of this runner (module-scoped `ChannelInitializerExtension` discovery not populated the way netty's real Maven/module build would)~~ — it was ONE MISSING SYSTEM PROPERTY. netty gates extension discovery behind `io.netty.bootstrap.extensions=serviceload` (`ChannelInitializerExtensions.java:55`), netty's own surefire `<argLine>` passes it (`pom.xml:1679`), and `common.args` did not. The SPI resource was present in `transport/target/test-classes` the whole time; nothing ever asked for it | Was: identical `AssertionFailedError: expected: <[id: 0x...]> but was: <null>` on both VMs. **RESOLVED 2026-08-29 by adding that one line to `common.args`: `BootstrapTest` `ok=17 failed=0` and `ServerBootstrapTest` `ok=6 failed=0` on BOTH VMs.** The row stays not-a-CratonVM-bug, for a different reason — nothing fails any more | `netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md` |

## What this list is not

It is not exhaustive over the full non-passing set — only classes actually
investigated as of 2026-08-19 appear here. It is not a claim that every row
was individually re-run on HotSpot: several `NativeImageHandlerMetadataTest`
and buffer-cluster rows are included on the strength of one representative
class's direct cross-check plus a shared, textually-identical failure
mechanism observed in the raw logs for the others — that's noted per row
above rather than glossed over. Anything still showing `FAIL`/`HANG`/`CRASH`
and *not* on this list should be treated as open until shown otherwise.

## A row can leave this list by being FIXED, not only by being wrong

The two HTTP/2 flow-controller rows above are struck through rather than
deleted, because the reason they left matters more than their absence would
say. "Not a CratonVM bug" was a correct verdict about the VM and a wrong
verdict about the *campaign*: the harness had never passed `-ea`, so netty's
own internal `assert` statements were no-ops and 12 tests could not pass on
either VM. That is a defect in the measurement apparatus, and it was
fixable — one line in `common.args`, validated with a full-suite A/B and 3
isolated reruns per arm. Both classes now pass.

Before adding a row here, ask whether the environment gap it names is one
this project could close. Several on this page genuinely are not (a native
library that is not installed, Maven metadata a fork-per-class runner has no
way to synthesize). The `-ea` one was, for a week, and nobody checked.

**It happened again, and it took ten days.** The two `bootstrap` rows blamed
"Maven metadata a fork-per-class runner has no way to synthesize" — the exact
phrase above — for a gap that was one system property netty's own surefire
configuration sets on the line right next to the `-ea` this page already
learned from. The SPI resource the row assumed was missing was sitting in
`transport/target/test-classes`.

So the check has a sharper form now: **do not describe the gap, LOOK for how
netty's own build closes it.** `grep` the reactor `pom.xml` for the property,
the argLine, the resource. Both times the answer was one line in `common.args`,
and both times the row's prose was a plausible story told instead of a search.

## Related

- `fail-hang-crash-rerun-20260817.md` — the rerun this index summarizes (removed 2026-08-19; its content is here and in the per-cluster docs).
- The per-cluster docs linked in the table above carry the full investigation
  for each row.
