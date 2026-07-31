# CratonVM — Tomcat 12.0 suite bug groups

Bug groups found running the complete Apache Tomcat 12.0 unit suite (651 JUnit
classes) under CratonVM vs HotSpot. Harness: `apps/tomcat/.tooling/run-suite.ps1`
(gitignored, local). HotSpot baseline: **635 PASS / 14 FAIL / 1 HANG /
1 NOSUMMARY** (the 16 non-PASS are all environmental — openssl/httpd binaries,
tribes multicast, flaky HTTP/2 — and are NOT counted as CratonVM bugs).

This directory groups the issues by root cause rather than per-class. Several
earlier groups have been FIXED and merged to `dev`; the dominant remaining wall
is interpreter throughput on embedded-server deployment.

Unified status (verified on the fresh dev worktree build, srun run):

| # | Bug / group | Repro class | Type | Status |
|---|-------------|-------------|------|--------|
| [01](01-jsse-tls-chain-FIXED.md) | JSSE/TLS chain (SSLContext→live HTTPS) | TestSsl | cluster | ✅ **FIXED** |
| [02](02-native-young-gen-oom-abort-FIXED.md) | Native alloc young-gen OOM hard-abort | (all server deploy) | CRASH | ✅ **FIXED** |
| [03](03-gc-root-snapshot-contention-FIXED.md) | GC root-snapshot lock contention | (all server deploy) | perf | ✅ **FIXED** |
| [07](07-beanelresolver-property-not-found-FAIL.md) | Introspector interface default-method property | jakarta.el.TestBeanELResolver | FAIL | ✅ **FIXED** |
| [08](08-importhandler-standard-packages-npe-FAIL.md) | ModuleFinder.ofSystem/jimage ModuleReader.list | jakarta.el.TestImportHandlerStandardPackages | FAIL | ✅ **FIXED** (peer) |
| [09](09-objectstreamclass-recordsupport-missing.md) | ObjectStreamClass$RecordSupport (record serialization) | catalina.realm.TestGenericPrincipal | NOSUMMARY | ✅ **FIXED** (peer) — TestJNDIRealm residual now FIXED in [13](13-hashtable-clone-cce-jndirealm-FIXED.md) |
| [13](13-hashtable-clone-cce-jndirealm-FIXED.md) | `Hashtable.clone()` casts synthetic native entry → CCE | catalina.realm.TestJNDIRealm | FAIL | ✅ **FIXED** |
| [04](04-embedded-server-throughput-wall-OPEN.md) | Embedded-server deployment throughput wall | (most catalina/coyote) | perf | 🔴 **OPEN** (dominant — most HANGs) |
| [06](06-openssl-ffm-clinit-segv-CRASH.md) | `Method.invoke` GC stale-ref under load (mis-blamed on OpenSSL FFM) | catalina.util.TestServerInfo | CRASH | ✅ **FIXED** — NOT an FFM bug (see note) |
| [10](10-pagecontext-npe-contains-null-FAIL.md) | Embedded-server serving wall (null response body; re-diagnosed → group 04, NOT a JSP/EL bug) | jakarta.servlet.jsp.TestPageContext | FAIL | 🔴 **OPEN** (→ 04) |
| [05](05-suite-rerun-fail-triage.md) | Remaining craton-only FAIL set — to triage | (~30 classes) | FAIL | 🔴 **OPEN** (mostly undiagnosed) |
| [14](14-classpath-url-protocol-not-registered-FIXED.md) | `classpath:` URL scheme unresolvable (`VM.isBooted` false → factory bypassed; pre-clinit factory publish; synthetic `URI.toURL` allowlist; webapp-TCCL resource scoping; `file:`-dir listing) | TestClasspathUrlStreamHandler, TestConfigFileLoader, TestPropertiesRoleMappingListener | FAIL | ✅ **FIXED** |
| [16](16-full-suite-6shard-rerun-20260721.md) | Full 646-class Linux 6-shard run + HotSpot control diff (corrected 2026-07-24 for a harness CWD bug) | **91 classes** (was miscounted as 23) | FAIL/HANG/CRASH | 🔴 **OPEN** (individually undiagnosed, like 05) |
| [18](18-fixture-environment-gaps-20260724.md) | True Linux fixture gaps (httpd/OCSP/LargeHeap/missing conf-Catalina-localhost/missing ant.jar), categorized by root cause | 35 classes | FAIL/HANG/CRASH | 🔴 **OPEN** (not CratonVM bugs — fixture completion work); its category J (9 classes, HANG-classification) turned out NOT to be a fixture gap at all — ✅ **FIXED**, see [hang-classification-unconfirmed-host-contention-FIXED.md](hang-classification-unconfirmed-host-contention-FIXED.md) |
| [21](21-tls-handshake-enforcement-gap-FIXED.md) | TLS handshake enforcement too loose/too strict (SNI, cipher/protocol allow-lists, client-cert) — **nine** distinct defects, not the one the doc assumed; incl. no TLS 1.2 server handshake ever completing through the SSLEngine, and a JDK static field `set_static_field` cannot write | 8 classes (TestSsl, TestSSLHostConfig{Compat,Cipher,Protocol}, TestSslHandshakeFailure, TestClientCert, TestCustomSslTrustManager, TestResolverSSL) | FAIL | ✅ **FIXED** (one by-design residual: `TestClientCert.testClientCertPostZero` — needs real renegotiation) |
| [22](22-tribes-realnetwork-membership-bug-FIXED.md) | Tribes real-socket group-membership undercounting — 3 causes: UDP recv timeout not typed `SocketTimeoutException` (so Tribes' receiver treated every idle poll as a failure and kept restarting membership); `bind` walked the whole address list, so a second bind to a live port silently took the other loopback family and every channel claimed port 4000; UDP socket `Mutex` held across `recv_from` delayed announcements by a poll interval | TestTcpFailureDetector, TestNonBlockingCoordinator | FAIL | ✅ **FIXED** (its one residual, the pre-existing TestGroupChannelSenderConnections SIGSEGV, is now also FIXED: [tribes-senderconnections-deserialize-segv-FIXED](tribes-senderconnections-deserialize-segv-FIXED.md)) |
| [23](23-charsetcache-pathological-slowdown.md) | `CharsetCache`'s "cached" path is 3x SLOWER than uncached | TestCharsetCachePerformance | FAIL/perf | 🔴 **OPEN** |
| [24](24-stringcache-oom-under-load-FIXED.md) | `StringCache.toString()` OOMs under sustained load at a heap HotSpot handles fine — NOT a StringCache bug: the GC-overhead limit scored every non-moving young sweep as "freed 0" (silently-reverted `a9c580aff` metric) | TestMethodPerformance | FAIL | ✅ **FIXED** — throughput residual tracked in [30](../../../known-issues/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md) |
| [25](25-charchunk-tostring-null-vs-empty-FIXED.md) | `CharChunk.toString()` returns `""` not `null` when empty/recycled — the Rust native decided null-vs-string from `buff == null`, but `AbstractChunk.recycle()` keeps `buff` and clears `isSet`, so a recycled chunk is `isNull()` *with a live buffer*. An audit of the rest of the `CharChunk` native family against HotSpot found 9 further contract divergences (null-argument NPE ×6, paired `c1 > 0xFF \|\| c2 > 0xFF` ignore-case folding ×3), plus 3 dead `force_native_over_real_jdk_bytecode` entries claiming natives that were never registered | TestCharChunk | FAIL | ✅ **FIXED** (2026-07-28) |
| [26](defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md) | Class-unload count off-by-one (9 vs 8) — recurrence of the 2026-07-14 fix, which was real but incomplete (whether it sufficed depended on whether the mirror had been promoted before the test's single `System.gc()`). TWO defects, both required: (a) `roots.rs` step 6 gated mirror deferral on the OLD-GEN-ONLY `metadata_pin_deferrable`, so every YOUNG user-loader `Class` mirror was rooted unconditionally and its `classLoader` edge pinned the loader; (b) a PARKED pool thread's per-thread JIT memo caches are published into its root snapshot, so an idle `http-nio-*-exec-N` pinned a (HashMap, node) pair from the JSP compile → JDT graph → `JspCompilationContext` → `JasperLoader` → the mirror | TestDefaultInstanceManager | FAIL | ✅ **FIXED** (2026-07-27) |
| [27](27-xxxendpoint-unix-domain-socket-init-failure-FIXED.md) | Unix domain socket connector init fails — `UnixDomainSocketAddress` was natively stubbed to throw even in real-JDK mode, and `{Server,}SocketChannel.open(ProtocolFamily)` had no CratonVM channel behind it; then, once bind/accept worked, `sc_read` held the `tcp_registry` read lock across a *blocking* `read()`, deadlocking the endpoint's acceptor against the in-VM client | TestXxxEndpoint | FAIL | ✅ **FIXED** (2026-07-27) |
| [28](28-http2-largeupload-byte-mismatch-FIXED.md) | HTTP/2 large POST truncated to one DATA frame — native `SSLEngine.unwrap` stopped scattering at the first FULL dst buffer (NOT flow control) | TestLargeUpload | FAIL | ✅ **FIXED** |
| [29](29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md) | Throughput-wall recurrence (HostConfig/Http2Section_8_2), relative-perf-assertion family, 2 contention-suspected, 1 Windows-only fixture gap | ~11 classes | mixed | ✅ **CLOSED** (2026-07-27) — it was NOT all throughput. Four real defects fell out of it, all FIXED: `File.setLastModified` returning `false` for directories; jar/war byte caches keyed on path only, so a redeployed archive served stale content; a truncated HTTP response body discarded instead of delivered; `file:`-URL leading-slash decided before percent-decoding. Ant classpath gap fixed on the Windows harness. The genuine throughput residue moved into [04](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md) |
| [30](../../../known-issues/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md) | Hot path fully interpreted: the loop method is OSR-denied by the RBC.7 `invokedynamic` ban (its trailing `println("…" + n)` string-concats), and `StringCache.toString` is refused by the RBC.6 handler-safety gate (its `synchronized` block's monitor handler) | TestMethodPerformance | perf | 🔴 **OPEN** (residual of 24) — also now carries tomcat/32's 32.4 and 32.3-SEQ2 residuals |
| [32](32-doc04-residual-perf-assertions-CLOSED.md) | The four per-test performance assertions carved out of 04. One real defect found and FIXED: the bulk `ByteBuffer` natives copied **one byte per accessor call** — 293 µs per 8 KiB where the same VM's `System.arraycopy` did it in 2.7 µs — which was the whole of the WebSocket SEQ1 gap and had been misattributed to "upcall and frame-decode cost". 32.2 is not a defect (now confirmed on a **loaded** host, 5.8 % margin). 32.1, 32.4 and 32.3's SEQ2 residual are consumers of VM-wide throughput problems and moved to the documents that own them | TestMapperPerformance, TestELParserPerformance, TestAsyncMessagesPerformance, TestOneLineFormatterPerformance | perf | ✅ **CLOSED** (2026-07-31) — **two of the four tests still FAIL**; they are tracked in [30](../../../known-issues/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md) and [the retired moving-young gate doc](../../jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md), not here |

14 of the diagnosed bug groups are FIXED (01/02/03/06/07/08/09/13/14/21/22/25/27/28); the open set is
dominated by the throughput wall (04) and the not-yet-individually-diagnosed
FAILs (05, 16).

> **Bug 06 — re-verified FIXED (2026-06-15).** The "OpenSSL Panama/FFM clinit →
> libffi SEGV" label was a wrong diagnosis. The crash was a GC stale-reference in
> the `java.lang.reflect.Method.invoke` wrapper (fix `9ce4c3b4`, in `dev`): the
> inner invoke could GC and move the `Method` mirror while a pre-call `args`
> snapshot held a now-dangling pointer, faulting under JUnit's per-method
> reflection — load-dependent, hence "intermittent, only under contention."
> Re-verified on `dev`: `TestServerInfo` = `OK (22 tests)`; **0 hard crashes
> across 30+ concurrent runs, JIT on AND off**; the `openssl_h` FFM `<clinit>`
> fails *cleanly* (caught `ExceptionInInitializerError`/NPE) with **no SEGV in
> either JIT mode** — the synthetic `panama.rs` wrong-ABI path is `synthetic-jdk`-
> only and off in the real-JDK suite build. Group 04's root cause was separately
> re-pinned to `update_root_snapshot` (~68% of a deploy) with two gated perf
> levers landed; it remains OPEN (perf, default-OFF gates).

## Current run status — fresh dev worktree (`srun`, `-Xmx2g`, 180s timeout)

Partial at time of writing (~220/651): **51 PASS / 32 FAIL / 136 HANG /
1 NOSUMMARY / 0 CRASH-so-far** (TestServerInfo/06 not yet reached). The **HANG
count dominates** = the throughput wall (group 04), not that many distinct broken
tests; some HANGs are also contention with concurrent runs. Craton-only
NOSUMMARY so far: `catalina.realm.TestJNDIRealm` (bug-09 family residual).

**The suite is NOT green.** Crash-class TLS/JCA/GC bugs + several semantic bugs
(07/08/09) are fixed; the remaining gap is (a) interpreter throughput for
server-test deployment (group 04, the bulk) and (b) the ~30 craton-only FAILs in
group 05 still to be individually diagnosed.

## Full 6-shard Linux run, 2026-07-21 (group 16)

Complete 646-class run on the Azure Linux host, `dev` @ `660985acb`, real
JDK 25 boot, `-Xmx2g`, 300s/class timeout, 6-way shard, plus a same-fixture
HotSpot control pass to separate real regressions from environment noise (see
[[reference_tomcat_triage_20260629]]'s rule — a HotSpot control pass is what
turned an apparent 184/646 FAIL count into 23 real ones):

**451 PASS / 23 confirmed CratonVM-only regressions / 172 fail on HotSpot too
(fixture gaps: missing `httpd`, missing OCSP-responder infra, `*LargeHeap`
needing a bigger `-Xmx` than the flat default) / 0 CRASH.**

Full breakdown and the 23-class list: [16](16-full-suite-6shard-rerun-20260721.md).
Reusable Linux runner: `apps/tomcat-suite-runner/run-tomcat-suite.sh`.

**Update, 2026-07-23:** merged `origin/dev` (253 commits, `→ 893ddbc73`),
rebuilt, and reran the 195 non-PASS classes in 4 shards. **12 of the 23
regressions are now fixed** upstream — **463 PASS / 11 confirmed regressions
remaining / 172 unchanged fixture gaps.** New finding: `TestNonBlockingAPI`
now reproduces a real Rust panic (`vm/src/runtime/value_stack.rs:237:25`,
`usize` underflow) on a background NIO worker thread — highest-priority item
in the remaining 11. Also flagged: two remaining-11 classes
(`TestMapperPerformance`, `TestJNDIRealmIntegration`) have existing docs in
this directory claiming they're already fixed, but both still fail/hang on
current `dev` — not reconciled yet. See group 16's addendum for full detail.

> **`TestJNDIRealmIntegration` reconciled 2026-07-26/27.** The class is 76/76
> with the JIT fully enabled on `com/unboundid/`, and its real producer
> (TOMCAT-JNDIREALM-JIT.3 — `string_case_cache` published only to the GC
> initiator, so any peer-initiated sweep reclaimed the cached case-conversion
> Strings) is fixed, with both JIT guards removed. **Verified on BOTH hosts**:
> 47 runs on the Windows box and 5 runs on the Azure Linux host against its own
> fixture, all `OK (76 tests)` with zero stale-pointer events — so its **HANG**
> row in the group-18/RESULTS-20260724 Linux table is resolved, not merely
> untested. See
> [jndirealmintegration-unboundid-jit-corruption-FIXED.md](jndirealmintegration-unboundid-jit-corruption-FIXED.md).
> `TestMapperPerformance` remains unreconciled.

> ⚠️ **The 172-fixture-gap figure above (both 2026-07-21 and 2026-07-23) is
> WRONG — corrected 2026-07-24.** `run-tomcat-suite.sh` never `cd`'d into the
> Tomcat checkout root before launching each test, so relative-path resource
> lookups (`new File("test/webapp")` etc.) silently resolved against the
> wrong directory on **both** VMs — that's why so many looked like
> "environment gaps." Fixed (one line, `cd "$TC_ROOT"`) and reran all 195
> non-PASS classes fresh. **Real split: 520 PASS / 91 confirmed CratonVM-only
> regressions / 35 true fixture gaps** (categorized by actual root cause in
> [18](18-fixture-environment-gaps-20260724.md)). See group 16's
> "CORRECTED addendum, 2026-07-24" for full detail — treat the 23/11/172
> numbers anywhere above this notice as historical, not current.

## Full 8-parallel LOCAL WINDOWS run, 2026-07-27/28 (groups 21-29)

Complete 646-class run on the local Windows box (`apps\tomcat`,
`apps\tomcat-suite-runner`, worktree `CratonVM-tomcat-full-suite-local-20260728`,
branch `test/tomcat-full-suite-local-20260728`), `dev` @ `60a710ad8`,
real JDK 25, 8-way `-Parallel`, 300s timeout: **547 PASS / 58 HANG / 40 FAIL
/ 1 NOSUMMARY**. Reran all 99 non-PASS classes at `-TimeoutSec 1500` (5x):
most HANGs turned out to be the known throughput wall finishing given
enough time — confirmed **31 classes still FAIL/HANG at 1500s that PASS on
a same-run HotSpot control** (7 of those still HANG even at 1500s).

Merged `origin/dev` again (`→ 12c79a0ee`, 250+ more commits) and rebuilt
before writing anything up — several classes in the 31 turned out to
already be fixed or explained by concurrent sessions' work (`TestDeployTask`
%20-decode fix, the Hashtable size-doubling fix that also explains
`TestGenerator`'s NPE, `*LargeHeap` needing `-XX:+UseG1GC -Xmx10g` per
[20](20-fixture-completion-regressions-closure-FIXED.md)) — reran the same
99 classes again on the fresh binary before finalizing. **8 genuinely new,
well-isolated CratonVM-only bugs** confirmed reproducing identically across
both binaries, written up individually: groups 21-28. Everything else
(throughput-wall recurrence, relative-performance-assertion family, 2
contention-suspected findings, 1 Windows-only fixture gap) is in
[29](29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md), not treated as new
bugs.
