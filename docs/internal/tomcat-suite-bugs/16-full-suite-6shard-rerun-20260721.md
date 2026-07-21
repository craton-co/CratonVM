# Group 16 — Full-suite 6-shard run (2026-07-21): 23 confirmed CratonVM-only regressions

Full 646-class Apache Tomcat JUnit suite, Azure host, `dev` @ `660985acb`,
worktree `wt-tomcat-full-suite-20260721` (binary `cratonvm-tomcat-full-suite-20260721`),
6-way sharded (one JVM per class, `org.junit.runner.JUnitCore <class>`), real
JDK 25.0.3 boot (`--java-home /home/victor/jdk25`), `--Xmx 2g`, 300s/class
timeout, `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1`.

**First pass: 450 PASS / 184 FAIL / 10 HANG / 2 NOSUMMARY / 0 CRASH.**

## Fixture gap found and fixed (not a CratonVM bug)

Before trusting the 196 non-PASS classes, found the Linux test fixture
(`/data/data/tomcat-dohead-fixture-20260717`, symlinked as
`/data/data/apps/tomcat`) had a completely **empty** `output/build/conf/` —
no `logging.properties`, `web.xml`, `server.xml`, `context.xml`,
`tomcat-users.xml`. Every `TomcatBaseTest`-based class hit
`FileNotFoundException: .../conf/logging.properties` during JULI setup,
cascading into `A child container failed during start`. Fixed by copying
`conf/*.{xml,xsd,properties}` from the checkout root into `output/build/conf/`
(safe as a plain copy — none of these files contain unsubstituted
`${ant-filterset}` tokens; verified with `grep -c '@[A-Za-z._]*@'` = 0 across
all of them). Reran the 196 non-PASS classes after the fix: only **1** flipped
to PASS. The conf gap explains almost none of the failures — don't spend more
time on it.

## HotSpot control pass (the actual signal)

Per [[reference_tomcat_triage_20260629]]'s rule — never trust a FAIL/HANG
count without a same-fixture HotSpot baseline — ran the same 196 classes under
real JDK 25 (`java`, no CratonVM) in the identical fixture, same sysprops/heap/
timeout. Result: **171/196 FAIL under HotSpot too.** The fixture is missing
`httpd`/OpenSSL/OCSP-responder test infrastructure and doesn't apply the
larger `-Xmx` Ant normally supplies for `*LargeHeap` classes — those fail
identically on both VMs and are NOT CratonVM bugs. Don't re-flag them without
completing the fixture first (installing `httpd`, wiring OpenSSL, adding the
per-class heap overrides Ant's `build.xml` `<test>` selectors apply).

Diffing HotSpot-PASS vs CratonVM-not-PASS over the 196:

| Bucket | Count | Meaning |
|---|---:|---|
| CratonVM regression (HotSpot PASS, CratonVM FAIL/HANG) | **23** | Real, worth investigating |
| Fixture/environment gap (fail on both VMs) | 172 | Not a CratonVM bug |
| CratonVM PASS, HotSpot FAIL | 0 | (none observed) |

Net for the whole 646-class suite: **450 + 1 (conf-fix rerun) = 451 clean
PASS**, **23 confirmed CratonVM-only regressions**, **172 pre-existing fixture
gaps**, **0 crashes**.

## 23 confirmed CratonVM-only regressions (HotSpot PASS / CratonVM not-PASS)

- `jakarta.el.TestBeanSupport` — FAIL
- `jakarta.el.TestOptionalELResolver` — FAIL
- `org.apache.catalina.authenticator.TestBasicAuthParser` — FAIL
- `org.apache.catalina.connector.TestResponsePerformance` — HANG
- `org.apache.catalina.core.TestApplicationFilterConfig` — FAIL
- `org.apache.catalina.core.TestSwallowAbortedUploads` — FAIL
- `org.apache.catalina.filters.TestAddCharSetFilter` — FAIL
- `org.apache.catalina.mapper.TestMapperPerformance` — FAIL
- `org.apache.catalina.nonblocking.TestNonBlockingAPI` — HANG
- `org.apache.catalina.realm.TestJNDIRealmIntegration` — FAIL
- `org.apache.catalina.util.TestURLEncoder` — FAIL
- `org.apache.catalina.valves.TestSSLValve` — FAIL
- `org.apache.coyote.http2.TestHttp2Limits` — FAIL
- `org.apache.coyote.http2.TestHttp2Section_8_2` — HANG
- `org.apache.el.TestValueExpressionImpl` — FAIL
- `org.apache.el.parser.TestELParserPerformance` — HANG
- `org.apache.juli.TestOneLineFormatterPerformance` — FAIL
- `org.apache.juli.TestThreadNameCache` — FAIL
- `org.apache.tomcat.util.buf.TestB2CConverter` — FAIL
- `org.apache.tomcat.util.buf.TestCharsetCachePerformance` — HANG
- `org.apache.tomcat.util.http.TestMethodPerformance` — FAIL
- `org.apache.tomcat.util.net.TestXxxEndpoint` — FAIL
- `org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance` — FAIL

Not yet individually root-caused (this doc is the worklist, like group 05).
Several share an obvious theme worth checking first:
- 5 are named `*Performance`/`*Section_8_2` — plausibly more of the group-04
  embedded-server throughput wall (interpreter-speed timeout under load)
  rather than a correctness bug; verify with `--nojit` off vs on and a longer
  per-class timeout before concluding FAIL/HANG is semantic.
- `TestBasicAuthParser`, `TestURLEncoder`, `TestB2CConverter`,
  `TestValueExpressionImpl`, `TestBeanSupport`, `TestOptionalELResolver` are
  non-server unit tests (no embedded Tomcat) — these are the cleanest,
  fastest to isolate, most likely genuine semantic bugs.

## Not real bugs — shared fixture gaps (172 classes, sample by category)

- **httpd integration** (`org.apache.tomcat.integration.httpd.*`, 8 classes):
  `FileNotFoundException: test/.../httpd-binary.lock` — no `httpd` binary
  configured on this host.
- **OCSP** (`org.apache.tomcat.security.TestSecurity2017Ocsp`,
  `org.apache.tomcat.util.net.ocsp.*`): `FileNotFoundException:
  test/.../ocsp-responder.lock` — same missing-infra pattern.
- **LargeHeap** (`*LargeHeap`, e.g.
  `catalina.tribes.group.interceptors.TestEncryptInterceptorLargeHeap`,
  `tomcat.util.buf.Test{Byte,Char}ChunkLargeHeap`): `OutOfMemoryError: Java
  heap space` at the harness's flat `-Xmx2g` — Ant's `<test>` selector applies
  a per-class larger heap override this harness doesn't replicate.
- The rest (jasper/`*.compiler.*`, `webresources.*`, `startup.TestHostConfig*`,
  most of `catalina.core`/`catalina.startup`) fail on HotSpot too in this
  specific fixture for reasons not individually triaged here — likely missing
  webapp resources, missing keystores, or CWD/relative-path assumptions the
  fixture's `output/build` layout doesn't satisfy. Fixing the fixture (proper
  `ant deploy` against a CWD of the real `apps/tomcat` checkout root, not a
  synthesized `output/build`) would very likely turn many of these into real
  PASSes on both VMs — worth doing before further Linux Tomcat suite runs.

## How to reproduce

```sh
F=/data/data/tomcat-dohead-fixture-20260717
EXE=/data/wt-tomcat-full-suite-20260721/target/release/cratonvm-tomcat-full-suite-20260721
CP="$(cat $F/.suite/cp-linux-fixed.txt):/home/victor/tomcat-build-libs/hamcrest-3.0/hamcrest-3.0.jar"
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1 \
$EXE --java-home /home/victor/jdk25 --Xmx 2g -Dfile.encoding=UTF-8 \
  -Djava.net.preferIPv4Stack=true -Dtomcat.test.basedir=$F/output/build \
  -Dtomcat.test.temp=$F/output/test-tmp -Dtomcat.test.tomcatbuild=$F/output/build \
  -Dtomcat.test.relaxTiming=true \
  --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.io=ALL-UNNAMED \
  --add-opens java.base/java.util=ALL-UNNAMED --add-opens java.base/java.util.concurrent=ALL-UNNAMED \
  -c "$CP" org.junit.runner.JUnitCore <class>
```

Raw per-shard `results.csv` + per-class logs for every non-PASS class from all
three passes (first full run, post-conf-fix rerun, HotSpot control) are on the
Azure host under
`/data/data/tomcat-dohead-fixture-20260717/.suite/results/{full-suite-20260721,rerun-after-conffix,hotspot-control}/`.
Worktree left in place (not cleaned up) for a follow-up session to triage the
23 individually.
