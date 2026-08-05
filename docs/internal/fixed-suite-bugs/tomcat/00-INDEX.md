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
| [23](23-charsetcache-pathological-slowdown.md) | `CharsetCache` cached paths lost to uncached because compiled Java calls were forced through generic dispatch; the direct-call gate, warm-native cache identity/policy/census, and wrongly typed `unmodifiableSortedMap` residual are repaired | TestCharsetCachePerformance | PASS/perf | ✅ **FIXED** (2026-08-01; exact JUnit `OK (1 test)`, full/lazy 0.386x/0.642x uncached; JIT/no-JIT semantic and cache-census probes green) |
| [24](24-stringcache-oom-under-load-FIXED.md) | `StringCache.toString()` OOMs under sustained load at a heap HotSpot handles fine — NOT a StringCache bug: the GC-overhead limit scored every non-moving young sweep as "freed 0" (silently-reverted `a9c580aff` metric) | TestMethodPerformance | FAIL | ✅ **FIXED** — throughput residual tracked in [30](../../tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md) |
| [25](25-charchunk-tostring-null-vs-empty-FIXED.md) | `CharChunk.toString()` returns `""` not `null` when empty/recycled — the Rust native decided null-vs-string from `buff == null`, but `AbstractChunk.recycle()` keeps `buff` and clears `isSet`, so a recycled chunk is `isNull()` *with a live buffer*. An audit of the rest of the `CharChunk` native family against HotSpot found 9 further contract divergences (null-argument NPE ×6, paired `c1 > 0xFF \|\| c2 > 0xFF` ignore-case folding ×3), plus 3 dead `force_native_over_real_jdk_bytecode` entries claiming natives that were never registered | TestCharChunk | FAIL | ✅ **FIXED** (2026-07-28) |
| [26](defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md) | Class-unload count off-by-one (9 vs 8) — recurrence of the 2026-07-14 fix, which was real but incomplete (whether it sufficed depended on whether the mirror had been promoted before the test's single `System.gc()`). TWO defects, both required: (a) `roots.rs` step 6 gated mirror deferral on the OLD-GEN-ONLY `metadata_pin_deferrable`, so every YOUNG user-loader `Class` mirror was rooted unconditionally and its `classLoader` edge pinned the loader; (b) a PARKED pool thread's per-thread JIT memo caches are published into its root snapshot, so an idle `http-nio-*-exec-N` pinned a (HashMap, node) pair from the JSP compile → JDT graph → `JspCompilationContext` → `JasperLoader` → the mirror | TestDefaultInstanceManager | FAIL | ✅ **FIXED** (2026-07-27) — but defect (a) was RE-OPENED by a default flip the next day; see [26c](defaultinstancemanager-third-recurrence-FIXED.md) |
| [26c](defaultinstancemanager-third-recurrence-FIXED.md) | Same off-by-one, THIRD occurrence, now **deterministic**. Defect (a) above was present and **inert**: its guard `young_marker_follows_side_tables()` factored `!moving_young_enabled()` across all three disjuncts, but the collector term it mirrors for an explicit `System.gc()` — `divert_non_moving`'s `explicit_full_gc` — carries no moving-young condition. `67de5400a` flipped `DEFAULT_MOVING_YOUNG` to `true` on 07-28, one day after the fix landed, and the predicate became unconditionally `false` on the shipped default. Proven by differential (same binary: default FAIL 2/2, `CRATONVM_NO_MOVING_YOUNG=1` PASS 2/2). The predicate is now written arm by arm against `divert_non_moving`, with the first unit tests it has ever had — asserting BOTH gate values, because a one-sided assertion passes before AND after such a flip | TestDefaultInstanceManager | FAIL | ✅ **FIXED** (2026-08-01) |
| [27](27-xxxendpoint-unix-domain-socket-init-failure-FIXED.md) | Unix domain socket connector init fails — `UnixDomainSocketAddress` was natively stubbed to throw even in real-JDK mode, and `{Server,}SocketChannel.open(ProtocolFamily)` had no CratonVM channel behind it; then, once bind/accept worked, `sc_read` held the `tcp_registry` read lock across a *blocking* `read()`, deadlocking the endpoint's acceptor against the in-VM client | TestXxxEndpoint | FAIL | ✅ **FIXED** (2026-07-27) |
| [28](28-http2-largeupload-byte-mismatch-FIXED.md) | HTTP/2 large POST truncated to one DATA frame — native `SSLEngine.unwrap` stopped scattering at the first FULL dst buffer (NOT flow control) | TestLargeUpload | FAIL | ✅ **FIXED** |
| [29](../../tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md) | Throughput-wall recurrence (HostConfig/Http2Section_8_2), relative-perf-assertion family, 2 contention-suspected, 1 Windows-only fixture gap | ~11 classes | mixed | ✅ **CLOSED** (2026-07-27) — it was NOT all throughput. Four real defects fell out of it, all FIXED: `File.setLastModified` returning `false` for directories; jar/war byte caches keyed on path only, so a redeployed archive served stale content; a truncated HTTP response body discarded instead of delivered; `file:`-URL leading-slash decided before percent-decoding. Ant classpath gap fixed on the Windows harness. The genuine throughput residue moved into [04](../../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md) |
| [30](../../tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md) | Hot path fully interpreted — **re-derived and CLOSED 2026-07-31**. All three named bans settled: RBC.7's premise removed, the `osr_dead_mask` gate hidden behind it LIFTED (the per-pc map the doc demanded was not needed), RBC.6 deliberately still narrow (re-widening costs a Spring wrong-locals defect and was A/B'd worth nothing), the ctor `putfield` ban default-lifted and regression-settled on the full 646-class suite. Loop control now at HotSpot parity. Every remaining item — its own residual plus 30.A/30.B adopted from 32 — root-caused to **one non-admission mechanism**: every `invokevirtual` from compiled code takes the generic dispatch helper (992 ns mono / 6027 ns poly vs HotSpot 5/9 ns), re-homed to [raw JIT-to-JIT](../../jit-raw-jit-to-jit-shadow-stack-overflow-FIXED-20260731.md). Turned up a **pre-existing silent wrong-result bug** on the way (a javac local slot reused across type categories) | TestMethodPerformance | perf | ✅ **CLOSED** (residual of 24) |
| [testssl-client-initiated-renegotiation](testssl-client-initiated-renegotiation-FIXED.md) | The last `TestSsl` failure left after the hostname-verification fix. The bare `assertTrue` is the `HandshakeCompletedListener` never firing — and probing it turned up **three** client-side JSSE defects unrelated to renegotiation: listeners accepted but never invoked (they had been registered as an inert no-op to silence an `AbstractMethodError`), `getSession()` returning **null** where JSSE guarantees non-null, and a version-pinned `SSLContext.getInstance("TLSv1.2")` ignored so the socket negotiated TLS 1.3. The protocol pin had to be fixed in `net_phase_e`'s `createSocket`, which re-registers the same triple later and **wins** — fixing only the `phases_late` copy changed nothing | TestSsl | FAIL | ✅ **FIXED** (2026-08-02) — one **by-design** residual: client-initiated TLS 1.2 renegotiation, which rustls omits as its CVE-2009-3555/3SHAKE mitigation, so `testClientInitiatedRenegotiation[JSSE]` stays red |
| [32](32-doc04-residual-perf-assertions-CLOSED.md) | The four per-test performance assertions carved out of 04. One real defect found and FIXED: the bulk `ByteBuffer` natives copied **one byte per accessor call** — 293 µs per 8 KiB where the same VM's `System.arraycopy` did it in 2.7 µs — which was the whole of the WebSocket SEQ1 gap and had been misattributed to "upcall and frame-decode cost". 32.2 is not a defect (now confirmed on a **loaded** host, 5.8 % margin). 32.1, 32.4 and 32.3's SEQ2 residual are consumers of VM-wide throughput problems and moved to the documents that own them | TestMapperPerformance, TestELParserPerformance, TestAsyncMessagesPerformance, TestOneLineFormatterPerformance | perf | ✅ **CLOSED** (2026-07-31) — **two of the four tests still FAIL**; they are tracked in [30](../../tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md) and [the retired moving-young gate doc](../../jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md), not here |
| [tls-stw](testsslhostconfigcompat-testhostec-read-timeout-FIXED.md) | `testHostEC[JSSE-KEYSTORE]` wedged for exactly 300 s in ~1 run in 4 — the https branch of `http_url_connection::perform` parked the calling thread in `recv()` with **no GC blocking region**, so a stop-the-world cross-thread JIT takeover deadlocked against it while every Tomcat thread that owed it bytes sat at the same barrier. Not an EC/no-SAN/endpoint-identification problem at all (the open doc's lead); `testHostEC` is simply test 12 of 78, where the run's Nth young GC lands | TestSSLHostConfigCompat | FAIL (flake) | ✅ **FIXED** (2026-08-01) |
| [locale-script](testacceptlanguage-locale-script-variant-dropped-FIXED.md) | `Locale.forLanguageTag` was a hand-rolled Rust BCP-47 split whose side table had no slot for a **script**, so `zh-hant-CN` came back as bare `zh_CN` ("expected:`<zh_CN_#Hant>` but was:`<zh_CN>`"). Four more defects in the same family: an extension singleton filed as the *variant* (`en-US-u-ca-japanese` → variant `"u"`), `und` treated as a language, `toString()` omitting the extension suffix, and `stripExtensions()` an identity stub. Fixed by delegating to the JDK's own `forLanguageTag` body — `Locale.Builder.setLanguageTag`, which shares that chain, was already byte-identical to HotSpot, so re-implementing BCP-47 in Rust was never needed. The Rust split survives for synthetic-JDK mode only, now with 8 unit tests | TestAcceptLanguage | FAIL | ✅ **FIXED** (2026-08-03) |
| [ws-async-seq2](websocket-async-send-interframe-latency-CLOSED-20260803.md) | The gap between two async WebSocket messages, opened as "root cause not found". It is not in the WebSocket code: `clearHandler` hands the `SendHandler` to the container `ThreadPoolExecutor`, so SEQ2's 500 us budget contains **two** AQS handoffs — and every AQS-mediated handoff in the VM is 13-26x HotSpot (`Condition.signal`→`await` 96.9 us vs 7.4; `execute`→task 167 us vs 7.9), while `park`/`unpark` and monitor `wait`/`notify` are within 3-4x. The doc's own probes measured Semaphore/LockSupport/Object.wait — none of which is the primitive on the path. One real defect fell out and is FIXED: **the invokestatic inline cache had been globally suppressed since the 2026-07-04 `loader_aware_resolution` default flip** (`loader_specific_dispatch` set on *attempting* loader-aware resolution, not on selecting an owner), so `Thread.onSpinWait()` — which AQS calls up to 255 times per handoff — cost 807 ns instead of 109 ns | TestAsyncMessagesPerformance | perf | ✅ **CLOSED** (2026-08-03) — **the test still FAILS**; the residue is the per-call floor and was carried by [aqs-thread-handoff-latency](../../aqs-thread-handoff-latency-RETIRED-20260805.md) — RETIRED 2026-08-05, funnel half fixed (leaf natives, 3.9-10.5x), test still blocked by the two thirds of an uncontended lock pair nobody has attributed: [uncontended-reentrantlock-pair-is-mostly-unattributed](../../../known-issues/vm/uncontended-reentrantlock-pair-is-mostly-unattributed-20260805.md) |

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
[29](../../tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md), not treated as new
bugs.

## Windows classpath fixture gap closed, 2026-08-03

`apps\tomcat\.suite\cp.txt` was short by four jars (BouncyCastle
provider/pkix/util 1.84 + EasyMock 5.6.0) because `Build-Classpath` matched
only Tomcat's renamed jar names and `$LIB` no longer exists on this box. Eleven
classes reported `NoClassDefFoundError` and had been filed as two separate
known-issues docs; both are now retired into
[bouncycastle-easymock-classpath-fixture-gap-FIXED](bouncycastle-easymock-classpath-fixture-gap-FIXED.md).
With the classpath repaired **CratonVM passes 13/13** of the affected classes
(JIT and `--nojit`) and **HotSpot passes 5/13** — EasyMock 5.6.0 cannot mock
classes on JDK 25 at all, so those 8 classes are permanently red in a HotSpot
control run and must not be scored as CratonVM regressions. The same doc
records two `run-one.ps1` defects found on the way (it passed none of the
suite's `--add-opens`/`tomcat.test.*` JVM args, and its exit code was always 0).

## Windows httpd reverse-proxy fixture closed — and a real defect under it, 2026-08-03

[httpd-proxy-integration-windows-FIXED-20260803](httpd-proxy-integration-windows-FIXED-20260803.md).
The 9 `org.apache.tomcat.integration.httpd.*` classes had been filed as a pure
fixture gap because HotSpot failed identically — but with no httpd installed
*both VMs fail before any VM-specific code runs*, so that shared red proved
nothing. Standing the fixture up (`setup-httpd-windows.ps1`: SHA-256-verified
Apache Lounge build into a local dir, plus a patch raising `TesterHttpd`'s
1000 ms listener deadline, which MPM WinNT startup misses every time at a
measured 1.0-1.5 s) produced **9 HotSpot PASS vs 9 CratonVM FAIL**, all on one
defect: the synthetic `Process` kept its own state at slots 0..5, which is
where real `java.lang.Process` bytecode resolves its own six
`inputReader`/`inputCharset`/… cache fields, so `p.inputReader()` read a pipe
fd as a `BufferedReader`. Now **9/9 on both VMs**. Regression witness:
`apps/tomcat-suite-runner/probes/ProcessReaderProbe.java`.

**The lesson to carry:** "HotSpot fails identically" only closes a family when
the shared failure is the *last* one. Until the fixture is actually up, it is
an untested hypothesis, not a verdict.


## IPv4-mapped IPv6 destinations unreachable on Windows, 2026-08-03

[teststartupipv6connectors-ipv6-mapped-ipv4-FIXED-20260803](teststartupipv6connectors-ipv6-mapped-ipv4-FIXED-20260803.md).
`TcpStream::connect*` takes its socket family from the `SocketAddr`, so a
`SocketAddr::V6` gets AF_INET6 — and Windows defaults `IPV6_V6ONLY` to 1, so it
cannot reach `::ffff:127.0.0.1` at all (WSAEADDRNOTAVAIL / os error 10049).
Linux defaults it off, hence Windows-only. Real JDK never builds that socket:
`InetAddress.getByName` returns an `Inet4Address`, and CratonVM's `InetAddress`
layer already mirrored the fold — **but every connect path that re-parses the
destination from a STRING in Rust bypasses it** (a URL's host text, or an
`InetSocketAddress` that kept its hostname). Folded at all six such sites via
`outbound_policy::normalize_connect_addr`. `TestStartupIPv6Connectors` 3/4 →
**4/4**, A/B over 16 network classes with **zero** regressions.

Two things generalise. **A one-line report can be a six-site defect:** the doc
named only `HttpURLConnection`; a probe over four connect surfaces found
`SocketChannel.connect` broken too. **And the dead-copy trap bit again** — the
first fix went into `http_client::open_connection`, which `HttpURLConnection`
does not use; `http_url_connection.rs` has its own connect loop, twice.

## WebSocket-over-TLS `[JSSE]` client connect fixed, 2026-08-03

`SSLEngine.wrap()` treated the caller's source buffer as application data for
one flight of the handshake: `do_wrap` gated on `!conn.is_handshaking()`, but
`handshake_status_of` keeps answering NEED_WRAP past that point (group 21's
TLS-1.2 server-flight fix), so a caller obeying NEED_WRAP had its buffer
drained — and the bytes ENCRYPTED onto the wire. Tomcat's WebSocket client
passes a static 16921-byte `DUMMY`, so `wrap` reported `bytesConsumed=16384`,
tripping `AsyncChannelWrapperSecure`'s "Bytes were consumed from the input
during a write", and the never-rewound `DUMMY` carried the damage into later
connections. Gated on `handshake_finished_reported` instead. Both `[JSSE]`
classes go FAIL → PASS and are the ONLY rows that move across a 16-class TLS
set run on two binaries; the residual `TestSsl` / `TestClientCert` failures are
the pre-existing renegotiation ones, identical on both arms. Write-up:
[websocket-jsse-wrap-consumed-app-data-during-handshake-FIXED](websocket-jsse-wrap-consumed-app-data-during-handshake-FIXED.md).

