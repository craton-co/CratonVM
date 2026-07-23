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
| [18](18-fixture-environment-gaps-20260724.md) | True Linux fixture gaps (httpd/OCSP/LargeHeap/missing conf-Catalina-localhost/missing ant.jar), categorized by root cause | 35 classes | FAIL/HANG/CRASH | 🔴 **OPEN** (not CratonVM bugs — fixture completion work) |

9 of the diagnosed bug groups are FIXED (01/02/03/06/07/08/09/13/14); the open set is
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
