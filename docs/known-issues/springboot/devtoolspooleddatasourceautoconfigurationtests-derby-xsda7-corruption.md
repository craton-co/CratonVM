# `DevToolsPooledDataSourceAutoConfigurationTests.inMemoryDerbyIsShutdown` — Derby `XSDA7` boot corruption

**Status: OPEN — investigated 2026-07-23. Deterministic, not load-related**
(reproduced 3× including an isolated `-Parallel 1` run on a quiet-ish host:
`SBRUNNER_RESULT tests=11 failed=1` every time, same test, same signature).
Distinct from the 2026-07-18 devtools closure's aastore-autobox fix (that
one produced `unknown_4294967295` sentinels; this is a genuinely different
symptom in the same test class).

## Symptom

The only test in the class that touches Apache Derby's real embedded engine
(`inMemoryDerbyIsShutdown`, `@Deprecated(since = "4.1.0")`, using
`jdbc:derby:memory:test;create=true`) fails during database boot:

```
Caused by: java.sql.SQLException: Failed to create database 'memory:test', ...
Caused by: ERROR XJ041: Failed to create database 'memory:test', ...
Caused by: ERROR XBM01: Startup failed due to an exception. ...
Caused by: ERROR XSDA7: Restore of a serializable or SQLData object of class , attempted to read more data than was originally stored
Caused by: java.io.EOFException: Unexpected EOF
	at org.apache.derby.iapi.services.io.FormatIdUtil.readFormatIdInteger(FormatIdUtil.java:66)
	at org.apache.derby.iapi.services.io.FormatIdInputStream.readObject(FormatIdInputStream.java:75)
	at org.apache.derby.iapi.services.io.FormatableHashtable.readExternal(FormatableHashtable.java:168)
	at org.apache.derby.catalog.types.IndexDescriptorImpl.readExternal(IndexDescriptorImpl.java:301)
	...
	at org.apache.derby.impl.sql.catalog.DataDictionaryImpl.create_SYSIBM_procedures(DataDictionaryImpl.java:11526)
	at org.apache.derby.impl.sql.catalog.DataDictionaryImpl.boot(DataDictionaryImpl.java:781)
```

It surfaces via `Mockito.spy()`'s real-method-call path (Hikari's
`PoolBase.newConnection` → `DriverDataSource.getConnection` →
`EmbeddedDriver.connect`), 10/11 other tests (mocked data sources, no real
Derby) pass every time.

## Working hypothesis (not confirmed with a debugger)

`jdbc:derby:memory:` is entirely in-memory — no disk I/O — so a truncated
read of previously-"stored" bytes means something wrote fewer bytes than
Derby's own deserializer expects into an **in-memory** buffer during the
*same* boot sequence, not stale on-disk state from a prior run.

The 2026-07-18 fix (`native-builtins/src/jdbc.rs`,
`register_derby_embedded_connection_native`) intercepts
`InternalDriver.connect(String, Properties, int)` for `jdbc:derby:memory:`
URLs and calls Derby's `getAttributes`/`getNewEmbedConnection` **directly,
synchronously, on the calling thread** — bypassing Derby's own
temporary-executor-based login-timeout mechanism entirely (done to dodge
Hikari's 30s connection timeout, since CratonVM's interpreted startup is
slower than HotSpot's). That means `DataDictionaryImpl.boot()` — which
would normally run on Derby's own dedicated connection-setup thread — now
runs on whatever thread called `dataSource.getConnection()` (here, Hikari's
pool-init thread). Derby's `Monitor`/`ContextManager` machinery leans
heavily on thread-scoped context state and (per a separate, already-fixed
CratonVM bug — see the June 23 "Derby XBM01 sync fix" commit `859a67305`,
about a cached-invokespecial dispatch path silently skipping monitor
acquisition for `synchronized` methods) on `synchronized` methods for
mutual exclusion during boot. A **plausible** but unconfirmed explanation:
running this boot sequence outside Derby's own expected thread/context
setup exposes a *different*, still-uncovered gap in how CratonVM handles
one of Derby's `synchronized`/thread-context-dependent code paths during
`DataDictionary` catalog creation, corrupting a reused in-memory
serialization buffer (`FormatableHashtable`/`IndexDescriptorImpl`) between
writing it and reading it back.

This has **not** been confirmed by attaching a debugger or adding
targeted tracing (that would be the natural next step — e.g. trace
`FormatableHashtable.writeExternal`'s byte count vs. what
`readExternal`/`FormatIdInputStream` actually reads back, and whether two
different `SYSIBM` procedure-creation calls interleave unexpectedly on the
same underlying buffer).

## Why no fix was attempted this session

The natural "safe" fix — spawning Derby's synchronous-bypass work on
CratonVM's own dedicated background thread (preserving Derby's "runs on its
own connection thread" invariant while still avoiding Hikari's timeout,
since a thread we control isn't bound by Derby's *internal* timeout either)
— requires native cross-thread invocation of Java methods with correct
GC-safety and result hand-off, which is a substantial, risky change to make
without being able to reliably verify it on this session's heavily
oversubscribed shared host. Given a similar risk was taken on the sibling
`DevToolsR2dbcAutoConfigurationTests$Embedded` bug this same session and
produced a *worse* failure (`StackOverflowError` cascade) than the original,
this one is left open rather than attempting an unverified fix.

## Reproduce

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList <tsv, header 'module\tclass', row 'module/spring-boot-devtools\torg.springframework.boot.devtools.autoconfigure.DevToolsPooledDataSourceAutoConfigurationTests'> `
  -RunName derby-repro -Parallel 1 -TimeoutSec 400
```

Reproduces every time (3/3 runs this session, including isolated
`-Parallel 1`), `SBRUNNER_RESULT tests=11 failed=1`.
