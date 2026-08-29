# TestManagerWebapp — bare assertion failures in deploy/servlet-listing tests

> ## ✅ 2026-07-10 FIXED — `testServlets` 100% fixed; `testDeploy`/`testBug57700`
> now blocked solely by the separate, already-tracked Group 04 deploy-throughput
> wall (moved to `docs/internal/`)
>
> Root-caused and fixed **six** independent, stacked bugs uncovered one layer
> at a time (each fix exposed the next; see the running log below for the
> exact isolated-probe evidence for each). `testServlets` now passes
> end-to-end. `testDeploy`/`testBug57700` still fail, but the remaining cause
> is precisely Group 04's already-documented interpreter-throughput wall
> (`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`)
> — a real webapp deploy now runs correctly (paths, JMX registration, and
> the deploy operation itself all work), it just doesn't finish inside any
> reasonable test timeout (confirmed >500s) on this shared, variably-loaded
> host, exactly matching that doc's own measurements. Per the known-issues
> triage rule, this doc retires since its residual is tracked by that
> separate, already-open doc.
>
> ### Bug 1 — `InetSocketAddress(String,int)` never resolved the hostname
> (dev commit `0e8c0df4`, landed independently by a concurrent session the
> same day — see [`form-authenticator-cookie-session-bare-assertion.md`](tomcat/form-authenticator-cookie-session-bare-assertion.md)
> for that write-up). The synthetic `InetSocketAddress.<init>(String,int)`
> native always left `addr=None`, so every request into any webapp
> configured with `RemoteCIDRValve` (Tomcat manager's default
> `../../../apps/META-INF/context.xml`) NPE'd on `request.getRemoteAddr()` — a bare 500
> on literally every request, which is what this doc's original symptom was.
>
> ### Bug 2 — `BaseModelMBean`'s JMX `invoke()` never reflected into the
> wrapped resource (`native-builtins/src/jmx.rs`, the synthetic
> `MBeanServer.invoke(ObjectName, String, Object[], String[])` native).
> It built a generic `(Ljava/lang/Object;)*Ljava/lang/Object;` descriptor
> from the params array's arity and called it directly on the registered
> MBean object — correct for a plain user MBean, wrong for a
> `DynamicMBean` wrapper like Tomcat's `BaseModelMBean` (which should
> reflect through to the real method on the *wrapped* resource, e.g.
> `HostConfig.tryAddServiced(String)`). Threw
> `NoSuchMethodError: BaseModelMBean.tryAddServiced(Ljava/lang/Object;)Ljava/lang/Object;`,
> blocking `ManagerServlet.tryAddServiced` (used by every deploy op).
> **Fix:** try delegating to the bean's own real
> `invoke(String, Object[], String[])` first (passing the real params/
> signature arrays through faithfully), falling back to the old
> generic-arity dispatch only if that fails — mirrors the
> `getAttribute`/`DynamicMBean` delegation pattern already used a few
> lines above it in the same file.
>
> ### Bug 3 — `ManagementFactory.getPlatformMBeanServer()` returns a
> bare-interface-typed `MBeanServer` (confirmed via
> `server.getClass()` printing literally `interface javax.management.MBeanServer`),
> not a concrete server, under the default-on `experimental-jmx` feature
> (`native-builtins/src/jmx.rs`'s `register_mbean_server` /
> `MBeanServerFactory.createMBeanServer` override — a deliberate design
> choice per the KAFKA-MBEAN note, since the real `JmxMBeanServer` chain
> genuinely throws under CratonVM today, see Bug 3a). Any un-overridden
> interface method throws `AbstractMethodError`. Fixed the specific gaps
> hit by Tomcat's manager webapp:
> - `addNotificationListener`/`removeNotificationListener(ObjectName,
>   NotificationListener, NotificationFilter, Object)` — added as no-op
>   stubs (the synthetic registry never broadcasts real notifications
>   anyway, so there's nothing to actually wire up; `StatusManagerServlet`
>   just needs the call to succeed).
> - `ObjectName.getKeyProperty(String)` — was entirely unregistered; real
>   bytecode for this method reads an internal `_ca_array` field the
>   synthetic 1-field `ObjectName` never populates, throwing
>   `NullPointerException: ... "this._ca_array" is null`. Added a native
>   parsing the stored canonical-name string directly.
> - `ObjectName.apply(ObjectName)` (wildcard/pattern matching) was
>   fundamentally broken — `pattern.contains('*')` short-circuited to
>   "matches everything" for **any** wildcard pattern, so
>   `queryMBeans("*:type=ThreadPool,*")` matched every registered MBean
>   (Engine, Realm, Mapper, ...), not just ThreadPools. Rewrote with real
>   JMX semantics: domain glob-matching, key-property subset/pattern
>   matching (`,*` / bare `*` = "any additional properties"), and
>   value-level globbing. Verified against 8 real-world pattern/candidate
>   pairs.
>
> ### Bug 3a (residual of Bug 3, separate deep root cause, NOT fixed) —
> The real `com.sun.jmx.mbeanserver.JmxMBeanServer` construction chain
> genuinely throws under CratonVM: an isolated probe calling
> `new MBeanServerBuilder().newMBeanServer(...)` directly (bypassing the
> synthetic override) hits
> `IllegalStateException: Can't register delegate` caused by
> `NullPointerException: "this._ca_array" is null` inside real
> `Repository.addNewDomMoi` / `ObjectName.getCanonicalKeyPropertyListString`.
> This is *why* the synthetic `MBeanServer` override exists in the first
> place (per the existing KAFKA-MBEAN note) — not something this session
> fixed, just confirmed and documented as the reason Bug 3's workaround is
> necessary rather than "just let the real bytecode run".
>
> ### Bugs 4-5 — `Diagnostics.getVMInfo()` (manager `vminfo` command) and
> `Diagnostics.getThreadDump()` (manager `threaddump` command) touch a
> long tail of MXBean methods that were individually unregistered on
> CratonVM's synthetic MXBean objects, each throwing `AbstractMethodError`
> the first time that code path was ever reached (previously masked by
> Bugs 1-3 crashing earlier). Fixed, in the order they surfaced:
> `RuntimeMXBean.getManagementSpecVersion()`, `getLibraryPath()`;
> `ThreadMXBean.isCurrentThreadCpuTimeSupported()`,
> `isObjectMonitorUsageSupported()`, `isSynchronizerUsageSupported()`,
> `getThreadCpuTime(long)`, `getThreadUserTime(long)`; `MemoryMXBean.isVerbose()`;
> `MemoryPoolImpl.getMemoryManagers0()` (backs `getMemoryManagerNames()`,
> called **unprotected** — no try/catch — by `Diagnostics`); and an
> entirely-missing `PlatformLoggingMXBean` (new synthetic type +
> registration, wired into `ManagementFactory.getPlatformMXBean`'s
> dispatch, where it previously fell through to `null` causing a
> `NullPointerException` instead of `AbstractMethodError`).
> `ThreadMXBean.dumpAllThreads(boolean,boolean)` was rewritten from
> "return an empty array" to build one (name-only) `ThreadInfo` per
> actually-live thread via the VM's existing `enumerate_threads` API
> (GC-safety: pins each thread object before dereferencing, re-reads the
> result array's address after each allocation) — needed because
> `Diagnostics.getThreadDump()`'s output is grepped for a real connector
> thread name substring (`"http-"`).
>
> ### Bug 6 — `OutputStreamWriter(OutputStream, CharsetEncoder)` (the
> constructor overload taking a pre-built `CharsetEncoder`, as opposed to
> a `Charset` or charset-name string) silently wrote **zero bytes** for
> **every** character, confirmed via an isolated probe testing all four
> constructor overloads side by side (`Charset`/`String` overloads: 1
> byte written correctly; `CharsetEncoder` overload, with or without
> explicit error-action configuration: 0 bytes, no exception). This is
> pure real JDK bytecode with no CratonVM native involved at all — a
> genuine VM-core interpreter bug, root cause not further diagnosed here.
> It silently corrupted `org.apache.catalina.util.URLEncoder.encode(String,
> Charset)` (which builds its `OutputStreamWriter` via exactly this
> constructor to percent-encode unsafe characters), dropping every
> percent-encoded character instead of emitting `%XX` — observed as every
> `/` in the manager's `war=<path>` deploy parameter being silently
> stripped (`.../examples` → `...examples`, squished into an invalid
> context path), which is what made `testDeploy` fail even after Bugs 1-2
> were fixed. **Fix (workaround, not a root-cause interpreter fix):**
> registered a native for this one constructor overload that delegates to
> the proven-working `(OutputStream, Charset)` constructor on the same
> object, reading the `Charset` off the caller's `CharsetEncoder` via its
> own real `charset()` accessor. Loses the caller's chosen
> malformed-input/unmappable-character error actions, which nothing in
> this codebase's test suites was observed to depend on.
>
> **Verification:** all 23 existing `native-builtins` JMX unit tests pass
> (`cargo test -p cratonvm-native-builtins --features experimental-jmx
> jmx_tests::`), including `test_management_factory_registration` (which
> specifically asserts `getPlatformMBeanServer` stays un-registered as a
> synthetic stub) and `test_total_registered_jmx_method_count`. The
> existing `vm/tests/wave3_b2_dispatch.rs` (`InetSocketAddress` round-trip)
> and `vm/tests/management_factory_clinit.rs` regression tests also pass.
> Isolated probes for every bug above (`MBeanProbe`, `MBeanBuilderProbe`,
> `ObjectNameKeyPropProbe`, `ObjectNameApplyProbe`, `WriterEncodeProbe`/
> `WriterEncodeProbe2`) were used to pin each root cause precisely before
> fixing — much faster than iterating on the full Tomcat suite per fix.

## Original summary

`org.apache.catalina.manager.TestManagerWebapp` failed `testDeploy` and
`testServlets` with bare `AssertionError` (no message):
```
1) testDeploy(org.apache.catalina.manager.TestManagerWebapp)
java.lang.AssertionError
2) testServlets(org.apache.catalina.manager.TestManagerWebapp)
java.lang.AssertionError
```
Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08). The
original doc's guess that this might be the *same* underlying issue as the
Group 04 deploy-throughput wall was **partially right** — it turned out to
be six independent functional bugs stacked on top of Group 04's real,
separate throughput ceiling, not the same thing as it.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName mgrwebapp `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.manager.TestManagerWebapp
```

Linux/Azure-host equivalent (harness assembled ad hoc under
`/data/data/apps/tomcat` + `/data/data/tomcat-build-libs`, classpath in
`.suite/cp.txt`; the `manager` webapp itself is not part of that harness's
`output/build/webapps` — stage it from `/tmp/tomcat-src-ref/webapps/manager`
and point `-Dtomcat.test.tomcatbuild=<dir>` at a directory containing both
`examples` and `manager`):
```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp.txt)
cratonvm --java-home /home/victor/jdk25 -Xmx2g \
  -Dtomcat.test.tomcatbuild=<dir-with-examples-and-manager-webapps> \
  -cp "$CP" org.junit.runner.JUnitCore org.apache.catalina.manager.TestManagerWebapp
# testServlets now passes. testDeploy/testBug57700 need a very long timeout
# (Group 04) to even attempt to complete their real webapp deploy.
```

## Residual (tracked separately, not by this doc)

`testDeploy`/`testBug57700`'s real webapp deploy now runs correctly but
doesn't finish inside any practical test timeout — this is Group 04's
interpreter-throughput wall, already tracked at
`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`.
