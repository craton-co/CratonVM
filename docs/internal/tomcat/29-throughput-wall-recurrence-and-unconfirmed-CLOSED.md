# Throughput-wall recurrence, relative-performance assertions, and unconfirmed findings — CLOSED

**Status: CLOSED, 2026-07-27.** Every item this doc listed has been resolved
into one of three buckets:

1. **Four genuine, separate CratonVM defects** that the doc had mis-filed as
   "throughput" or "unconfirmed" — all root-caused and **FIXED** on branch
   `fix/tomcat-doc29-closure-20260727` (see *Fixes* below).
2. **One harness fixture gap** (`ant.jar` missing from the Windows classpath) —
   fixed on this host, and the requirement is now written into the tracked
   harness doc so it cannot silently regress on a fresh fixture.
3. **The genuine group-04 throughput residue** — re-anchored into
   [`04-embedded-server-throughput-wall-OPEN.md`](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md),
   which now carries the 1500 s-rerun evidence table. Those classes are
   measurements of a known open problem, not bugs of their own.

Nothing from this doc remains open here. The original triage text is preserved
at the bottom for provenance.

---

## Fixes

### 1. `java.io.File.setLastModified` silently failed for DIRECTORIES

*Symptom:* `TestHostConfigAutomaticDeploymentUpdateWarOffline` — all four
`testUpdateWarOfflineContext{FF,FT,TF,TT}` fail with
`AssertionError: Failed to set last modified for [...\webapps\myapp]`.
Deterministic, reproduced identically in both reruns, passes on HotSpot.

*Root cause:* `../../../native-builtins/src/phases_late/nio_file.rs` implemented the
`java/io/File.setLastModified(J)Z` native as
`OpenOptions::new().write(true).open(path)` + `File::set_modified`. That
spelling cannot open a **directory** on any platform — Windows `CreateFileW`
refuses a directory handle without `FILE_FLAG_BACKUP_SEMANTICS`, POSIX
`open(2)` returns `EISDIR` — so the native returned `false` for every
directory. `doTestUpdateWarOffline` ages the *expanded webapp directory* with
`dir.setLastModified(...)` and asserts the return value.

A second, latent instance of the same gap sat next to it: the
`WinNTFileSystem`/`UnixFileSystem` `setLastModifiedTime(Ljava/io/File;J)Z`
native (the one real-JDK `File.setLastModified` bytecode delegates to) was a
stub that returned `true` **without touching the file at all**.

*Fix:* both entry points now go through one helper, `set_file_mtime_millis`,
built on `filetime::set_file_mtime` (already a direct dependency of
`native-builtins`), which opens with the correct per-platform flags and works
for files and directories alike.

### 2. Jar/WAR byte caches were keyed on PATH ONLY — a redeployed archive served stale content forever

*Symptom:* `TestHostConfigAutomaticDeploymentUnpackWAR.testUnpackWARTTF` fails
with `expected:<true> but was:<false>` — the WAR is never expanded into a
directory even though `HostConfig.deployWAR` logs a successful deployment and
no error is raised anywhere.

*The tell:* the method **passes when run alone** (verified: `rc=0` in 501 s)
but fails as soon as it runs after another method **in the same class
fixture**. Minimal in-class repro — just two of the class's eight methods,
`testUnpackWARFFF` then `testUnpackWARTTF`:

```
CratonVM (stock dev)  RunMethods] ran=2 failed=1 (884696ms)
                      FAILURE testUnpackWARTTF: expected:<true> but was:<false>
HotSpot (same pair)   RunMethods] ran=2 failed=0 (9492ms)
CratonVM (fixed)      RunMethods] ran=2 failed=0 (1096563ms)
```

That is fixture-state pollution, not a functional bug in the deploy path.

*Root cause:* four separate caches memoise archive bytes/indexes keyed by
**absolute path with no invalidation**:

| cache | file |
|---|---|
| `cached_outer_jar` | `../../../native-builtins/src/net_phase_e.rs` |
| `cached_nested_jar` | `../../../native-builtins/src/net_phase_e.rs` |
| `jar_bytes_cached` | `../../../native-builtins/src/phases_late/nio_file.rs` |
| `jar_index` | `../../../native-builtins/src/phases_late/nio_file.rs` |

They were introduced to stop a Spring Boot fat-jar autoconfig walk from
re-reading a 100 MB archive per lookup, on the assumption that a classpath jar
is immutable for the VM's lifetime. **That assumption does not hold for an
application server.** Tomcat's auto-deployer replaces `<appBase>/<app>.war` in
place and redeploys it, and this test class does exactly that:
`HostConfigAutomaticDeploymentBaseTest.createWar()` writes a *different* WAR to
the *same* `<appBase>/myapp.war` for each `@Test` method.

*Minimal repro, no Tomcat needed* (`JarRedeployProbe`: write a jar, read one
entry through a `jar:file:…!/…` URL, rewrite the SAME path with different
content of the SAME length, read again):

```
HotSpot   URL.openStream : first=AAAA second=BBBB  -> OK
CratonVM  URL.openStream : first=AAAA second=AAAA  -> STALE      <- the bug
CratonVM  JarFile        : first=AAAA second=BBBB  -> OK
```

Note which path is stale. That asymmetry is exactly what made the Tomcat
failure so confusing: `HostConfig.deployWAR` reads the WAR's
`../../../apps/META-INF/context.xml` through `new JarFile(war)` — the FRESH path — and
correctly sets `unpackWAR=true`. But it then records
`context.setConfigFile(UriUtil.buildJarUrl(war, …))`, and at context start
`ContextConfig.processContextConfig` **re-parses that same descriptor through
the URL** (`contextXml.openConnection().getInputStream()`, with an explicit
`setUseCaches(false)` that CratonVM's cache ignored) — the STALE path. The
re-parse handed back the FIRST method's `unpackWAR="false"`, overwriting the
correct value, so `fixDocBase` skipped `ExpandWar.expand` and the expanded
directory never appeared. No error is logged anywhere because nothing failed —
the VM simply answered an older question.

Blast radius is much wider than this one test: **any** hot redeploy of a
jar/war at a stable path inside one JVM lifetime got the old archive, and any
caller that explicitly asks for `setUseCaches(false)` was silently ignored.

*Fix:* all four caches are now keyed on `(path, archive_stamp(path))`, where
`archive_stamp` is `(mtime_nanos, len)` from a single `fs::metadata` call, and
each insert evicts older generations of the same path so a long-running server
that redeploys repeatedly does not accumulate every past version's bytes. One
`stat` per lookup is negligible next to the multi-MB read + zip re-parse these
caches exist to avoid.

### 3. A truncated HTTP response body was DISCARDED instead of delivered

*Symptom:* `org.apache.jasper.compiler.TestGenerator.testBug56581` fails with
`NullPointerException: Cannot invoke "String.startsWith(String)" because
"result" is null`. This doc had it filed as "may already be fixed by the
Hashtable change, needs a focused rerun to confirm either way". It is **not**
fixed — an isolated single-method rerun on current `dev` reproduces it in
~150 s, deterministically.

*Root cause:* `bug56581.jsp` writes 1000 lines, commits the response, then
throws; `ErrorReportValve` aborts the connection mid-body. The test asserts on
the 1000 lines the client **did** receive *and* on the resulting `IOException`.
CratonVM's native HTTP client
(`native-builtins/src/http_url_connection.rs::read_chunked`) turned "socket
closed mid-chunk" into a hard `Err`, throwing away every byte already decoded,
so `ByteChunk.toString()` returned `null`. HotSpot's semantics are the
opposite: `getResponseCode()` succeeds, `getInputStream()` hands out what
arrived, and only the read that runs past the truncation point throws.

*Fix:* truncation is recorded on a thread-local side channel
(`LAST_RESPONSE_TRUNCATED`, mirroring the existing `LAST_REASON_PHRASE`
pattern) instead of destroying the body, at all three truncation sites
(mid-chunk-header, mid-chunk-body, short of `Content-Length`). When the flag is
set, `getInputStream()`/`getErrorStream()` return
`SequenceInputStream(ByteArrayInputStream(partial), new PipedInputStream())` —
the unconnected `PipedInputStream` is a real JDK `InputStream` whose first read
throws `IOException`, so a Java reader sees exactly HotSpot's shape: the bytes
that arrived, then the error. Real JDK classes are used rather than a CratonVM
synthetic because callers wrap the result in `BufferedInputStream`, which is
typed against `java.io.InputStream`.

### 4. `file:` URL leading slash + percent-decode ordering (`os error 123`)

*Symptom:* with the classpath gap below fixed, `TestDeployTask` finally runs
and `bug58086a` fails with
`URL.openStream: read jar /C:/craton/.../dir with spaces/context.jar: … (os error 123)`.

This doc predicted the `%20` half of this bug was already fixed on `dev` — it
is, but only half. `net_phase_e.rs`'s `URL.openStream` jar path decides whether
to strip the `file:`-URL leading slash before a Windows drive letter by probing
`Path::exists()` — **on the still-percent-encoded string**. A directory
literally named `dir with spaces` appears in the URL as `dir%20with%20spaces`,
which never exists on disk, so the probe failed and the code fell back to the
raw form, handing Windows `/C:/…` — "The filename, directory name, or volume
label syntax is incorrect".

*Fix:* the probe now runs on the percent-decoded path, and when neither
candidate exists a Windows drive-letter shape (`X:/…`) prefers the trimmed form
so the failure surfaces as a proper `FileNotFoundException` rather than a
syntax error.

---

## Harness fixture gap (not a CratonVM bug)

`org.apache.catalina.ant.TestDeployTask` and `org.apache.jasper.TestJspC` need
Ant's own `ant.jar` + `ant-launcher.jar` on the test classpath — `DeployTask`
and `org.apache.jasper.JspC` both extend `org.apache.tools.ant.Task`. Without
them both classes die with `NoClassDefFoundError: org/apache/tools/ant/Task`
on **HotSpot as well as CratonVM**. The Linux fixture was fixed on 2026-07-23;
this Windows host had not been.

Fixed here by adding Ant to the Windows harness's `Build-Classpath`
(`apps\tomcat-suite-runner\run-tomcat-suite.ps1`, which is host-local and
**not** git-tracked — the tracked `run-tomcat-suite.md` now documents the
requirement so a fresh fixture cannot silently miss it) and appending both jars
to `apps\tomcat\.suite\cp.txt`. That same `Build-Classpath` now also searches
the Gradle module cache in addition to the long-gone
`C:\Users\Victor\tomcat-build-libs`, so re-running `-Setup` no longer destroys
a working `cp.txt`.

HotSpot control after the fix: `TestDeployTask` `OK (5 tests)`, `TestJspC`
`OK (11 tests)` — i.e. the fixture gap, not the VM.

---

## Residual, correctly owned by group 04

The classes below are *evidence of the known embedded-server throughput wall*,
not bugs of their own, and now live in
[`04-embedded-server-throughput-wall-OPEN.md`](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md)
with their measured numbers: `TestHostConfigAutomaticDeployment{Addition,
Modification,DeleteC}`, `TestHttp2Section_8_2`, `TestMapperPerformance`,
`TestELParserPerformance`, `TestAsyncMessagesPerformance` and
`TestOneLineFormatterPerformance`. That doc also records *why* the last two
invert their "the optimised path should win" assertions on CratonVM
(`java.util.Formatter.format` — what `String.format` delegates to — is a Rust
`NativeKind::Intrinsic` racing plain interpreted bytecode; and the ELParser
test's first loop absorbs JIT warm-up).

One class this doc listed is owned elsewhere:
`org.apache.tomcat.util.http.TestMethodPerformance` fails with
`OutOfMemoryError` inside `StringCache.toString` — that is
[`24-stringcache-oom-under-load.md`](../../../known-issues/tomcat/24-stringcache-oom-under-load.md), not a
timing assertion.

---

## Verification summary

All on this Windows box, CratonVM binaries built from
`fix/tomcat-doc29-closure-20260727` (`-base-` = stock `dev` 234a5a273,
`-fix3-` = the final branch state). Note the wall-times: the box was carrying
other sessions' builds throughout, which is why the Tomcat classes take
minutes-to-hours; the PASS/FAIL verdicts are unaffected.

| check | stock `dev` | fixed | HotSpot |
|---|---|---|---|
| `JarRedeployProbe` — `jar:` URL after in-place rewrite | `STALE` | `OK` | `OK` |
| `JarRedeployProbe` — `JarFile` | `OK` | `OK` | `OK` |
| `SetLastModProbe` — directory | `false` | `true` | `true` |
| `SetLastModProbe` — non-existent path | `false` | `false` | `false` |
| `FileStateProbe` (control: stat not cached) | `ALL OK` | `ALL OK` | `ALL OK` |
| `TestGenerator#testBug56581` | NPE | `rc=0` (35.8 s) | pass |
| `TestDeployTask` | `NoClassDefFoundError` → `os error 123` | `OK (5 tests)` (586 s) | `OK (5 tests)` |
| `TestJspC` | `NoClassDefFoundError` | — | `OK (11 tests)` |
| `UnpackWAR` `FFF`+`TTF`, ONE fixture | `failed=1` (885 s) | `failed=0` (692 s) | `failed=0` (9.5 s) |
| `UpdateWarOffline` (full class) | 4 failures, 8× `Failed to set last modified` | `OK (8 tests)`, 0× | pass |
| `TestNonBlockingAPI` (full class, no timeout) | — | `Tests run: 44, Failures: 1` (contention, see below) | pass |
| `bt18` GC oracle | `68332206`, 2823/2817/3546 ms | `68332206`, 2765/2912/2863 ms | `68332206` |

The `bt18` line is the GC-correctness gate: the truncated-stream fix holds a
heap reference across two constructor up-calls, so it pins and re-reads through
`pin_native_root`/`read_native_pin`. Checksum matched HotSpot on every run and
the interleaved timings are a wash.

## Reproduction / verification

Single class or single method, replicating the suite runner's env exactly
(`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1`, `-Xmx2g`, CWD
`apps\tomcat`):

```powershell
# whole class
cratonvm.exe -Xmx2g -cp (gc apps\tomcat\.suite\cp.txt) org.junit.runner.JUnitCore <fqcn>
# one method (RunSingleJUnit) / an ORDERED subset (RunMethods) — both in apps\tomcat\.suite
cratonvm.exe -Xmx2g -cp "<cp>;apps\tomcat\.suite" RunMethods <fqcn> <method1> <method2>
```

`RunMethods` is what pinned down fix 2: a method that passes alone but fails in
a full-class run is process/fixture-state pollution, and running exactly two
methods costs ~15 min instead of the ~2.5 h the whole class needs.

**Trap worth knowing** — the obvious implementation of such a runner (a loop of
`JUnitCore.run(Request.method(cls, m))`) is *useless* for this: each `Request`
re-runs `@BeforeClass`, and Tomcat's `LoggingBaseTest.setUpPerTestClass`
creates the per-class temp directory there. Every method therefore gets a
FRESH `appBase` and the very same-path rewrite you are hunting never happens —
the first version of this tool reported a clean PASS for a pair that fails in
the real class run. `RunMethods` uses ONE `Request.aClass(cls).filterWith(…)`
so the class fixture is shared, exactly as in a full run.

---

## Original triage text (2026-07-24, preserved)

Not new bugs — grouped here for completeness of this rerun's accounting,
distinct from the 8 genuinely new, well-isolated bugs in the sibling docs
(21-28) in this batch.

### Already-known, OPEN "embedded-server throughput wall" reconfirmed at larger scale

`04-embedded-server-throughput-wall-OPEN.md` already documents CratonVM's
per-request/per-deploy interpreter overhead vs. HotSpot. This 1500s rerun
reconfirms it compounds badly in classes with many sequential embedded-server
start/stop or deploy/undeploy cycles — evidence, not new:

- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition` — a
  single webapp directory deploy taking **107.9 seconds**.
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentModification` —
  a single deployment-descriptor deploy takes **109.7 seconds**.
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteC` —
  still hangs at 1500s in both runs (its sibling `…DeleteB` passed at 1365s in
  the second run — a matter of degree, not a qualitatively different defect).
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentUnpackWAR` /
  `…UpdateWarOffline` — both eventually FAIL (not hang) after 890-962 seconds —
  worth a closer look at whether the failure is itself throughput-induced or a
  separate assertion, but not investigated further here.
  **→ Resolved: neither is throughput. Fixes 1 and 2 above.**
- `org.apache.coyote.http2.TestHttp2Section_8_2` — 1000+ parameterized
  sub-cases, each starting and stopping its own embedded connector — same
  multiplication effect, not a deadlock.

### Relative-performance-assertion family — likely same throughput ceiling, different assertion style

- `org.apache.catalina.mapper.TestMapperPerformance.testPerformance` —
  `AssertionError: 40823`. **→ Actually an ABSOLUTE 5 s budget, not a relative
  assertion; the 40823 reading came from a contended host. On an idle box the
  easiest hostname now fits (4.1 s) and only the hardest misses (7.0 s),
  i.e. 19-41× HotSpot against a fixed limit.**
- `org.apache.juli.TestOneLineFormatterPerformance.testDateFormat` —
  `AssertionError: String#format was faster that DateFormatCache`. **→
  `String.format` is a Rust intrinsic on CratonVM (2.8× HotSpot) racing plain
  bytecode (73-293×); the asymmetry alone flips the assertion. Numbers in
  group 04.**
- `org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance.testAsyncTiming` —
  plain `AssertionError`. **→ Frame sizes are correct; only the inter-chunk
  timing counters trip. Pure per-write latency, not a framing bug.**
- `org.apache.el.parser.TestELParserPerformance.testParserInstanceReuse` —
  inconsistent across runs. **→ The first loop absorbs JIT warm-up; the two
  totals land within ~1 % of each other.**

> **Update 2026-07-27 (from `dev`) — one member of this family is now
> root-caused, and it is NOT a diffuse interpreter ceiling.**
> `org.apache.tomcat.util.http.TestMethodPerformance` was chased down to two
> *named* JIT-admission gates that leave its entire hot path interpreted: the
> loop method is permanently OSR-denied by the RBC.7 `invokedynamic` ban
> (triggered by its trailing `println("…" + duration + "ns")` string-concats),
> and `StringCache.toString` is refused outright by the RBC.6
> exception-handler-safety gate (triggered by its `synchronized` block's
> javac-generated monitor handler). Full analysis, probe table, and fix
> directions:
> [30](30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md).
> **Worth running the same check over the classes above** —
> `CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1` names the gate in one run. It is
> the obvious next step for `TestMapperPerformance`, whose 19–41× gap on a
> pure-compute `mapper.map()` loop is exactly the shape a permanently denied
> hot method produces.

### Unconfirmed / contention-suspected

- **`org.apache.jasper.compiler.TestGenerator`** — NPE in `testBug56581`,
  suspected already fixed by the Hashtable-size-doubling fix. **→ NOT fixed;
  reproduced in isolation on current `dev`. Fix 3 above.**
- **`org.apache.catalina.nonblocking.TestNonBlockingAPI`** — PASSED at 438s in
  the first run, HANGed at 1500s in the second, on a host running several other
  concurrent build/test sessions. **→ CONTENTION, confirmed.** Rerun here on a
  host carrying 18+ concurrent `rustc` processes from other sessions, with no
  harness timeout: the class **ran to completion**, `Tests run: 44,
  Failures: 1` — it does not hang, it simply does not fit inside a 1500 s
  budget when the box is busy. The single failure
  (`testNonBlockingReadChunkedSplitMaximum`, `java.net.SocketException:
  Connection aborted: write0 … (os error 10053)` thrown from
  `SimpleHttpClient.sendRequest`, i.e. a raw-`Socket` CLIENT WRITE) is
  contention too: run on its own that method PASSES on both the stock and the
  fixed binary with identical timing (`rc=0`, 43.0 s vs 43.6 s).

### Not a CratonVM bug at all — Windows-harness fixture gap

- **`org.apache.catalina.ant.TestDeployTask`** — `NoClassDefFoundError:
  org/apache/tools/ant/Task`. **→ Fixed, see the harness section above; and it
  then revealed fix 4.**
