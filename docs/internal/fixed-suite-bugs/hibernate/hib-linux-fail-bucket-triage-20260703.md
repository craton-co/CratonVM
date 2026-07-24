# Hibernate FAIL-bucket triage on Linux (Azure host, dev `0d142fad`+, 2026-07-03)

**Status (2026-07-06): all classes tracked by this doc are now FIXED or ruled
out.** See "Fixed 2026-07-06" below — the 8 classes in "Genuinely new" (10
counting the 3-class timezone bullet) were re-verified against current dev
(Windows box, `9f1db39d`+) and all pass. Several turned out to already be
fixed by unrelated earlier work this week (softdelete CHM-order fix, NoDepth
ServiceLoader fix, HIB-CV-34 timezone fix); one (`CacheKeyEmbeddedIdEnanchedTest`)
had a genuine, previously-undiagnosed bug, root-caused and fixed this
session; one (`MetadataAccessTests`) was never actually broken — the earlier
"HANG" observation was a batch-position timeout artifact, not a real hang.
This doc is being archived to `..` per the known-issues triage
rule (no open defect remains).

Source: the 387-class non-passed set from a full-suite Linux run (real-JDK,
JIT on, TIMEOUT=600), OSR-off baseline (239 FAIL). After excluding the
already-tracked clusters (bytecode.enhancement/lazytoone ~187, jar-scanning 3,
temporal-GC 8, cascade-null 2), **39 classes remained unclassified**. Triaged
below into known-but-broader, environment/packaging (ruled out), and
genuinely new bugs.

## Fixed 2026-07-03 (branch `fix/linux-jdbc-serviceloader-and-javac-cp`)

### A. `NoClassDefFoundError: com/microsoft/sqlserver/jdbc/SQLServerDriver` (3 classes) — FIXED
`connection.DriverManagerRegistrationTest`, `dialect.resolver.DialectFactoryTest`,
`dialect.resolver.DialectSpecificConfigTest`.

**Root cause was NOT ServiceLoader/jar-scanning** (the original hypothesis) —
`ServiceLoader.load(java.sql.Driver.class)` correctly discovered all 12
providers across the classpath's jars, including `SQLServerDriver`, sorted
and deduped. The actual failure: `com.microsoft.sqlserver.jdbc.SQLServerDriver
.<clinit>` transitively initializes `jdk.net.ExtendedSocketOptions`, which on
Linux resolves its platform implementation to `jdk.net.LinuxSocketOptions`
(the Windows JDK has no such class — it uses `WindowsSocketOptions`, which
CratonVM already stubs). `LinuxSocketOptions.quickAckSupported()` calls a
native `quickAckSupported0()Z` CratonVM never registered → `UnsatisfiedLinkError`
(uncaught, since it's an `Error`, not wrapped by `ExceptionInInitializerError`)
→ propagates through `SQLServerDriver.<clinit>`, marking the class
**erroneous** per JVMS §5.5. The first touch (inside `ServiceLoader`'s
provider-instantiation loop, itself silently swallowing per-provider
failures — real JDK `DriverManager` behavior) throws `UnsatisfiedLinkError`
with no visible trace; every subsequent reference (the test's own explicit
use of the driver) gets `NoClassDefFoundError` instead, with no
`ExceptionInInitializerError` anywhere in the log — exactly the symptom
originally observed. Confirmed via a direct `Class.forName` probe capturing
the full cause chain:
```
UnsatisfiedLinkError: jdk/net/LinuxSocketOptions.quickAckSupported0()Z
    at jdk.net.LinuxSocketOptions.quickAckSupported(LinuxSocketOptions.java:51)
    at jdk.net.ExtendedSocketOptions.<clinit>(ExtendedSocketOptions.java:233)
    at com.microsoft.sqlserver.jdbc.SQLServerDriver.<clinit>(SQLServerDriver.java:1200)
```
**Fix**: `../../../../native-io/src/net.rs` — added a `jdk/net/LinuxSocketOptions` native
block mirroring the existing `jdk/net/WindowsSocketOptions` one (same
"report unsupported" policy: `keepAliveOptionsSupported0`/`quickAckSupported0`/
`incomingNapiIdSupported0` → `false`, IP_DONTFRAGMENT/TCP_KEEPALIVE
getters/setters → safe no-op defaults, `getSoPeerCred0` → `-1`). Platform-
neutral by construction: this class doesn't exist on the Windows JDK, so the
registration is inert (never looked up) there.

### B. HIB-CV-27 (in-process javac / H2 dynamic stored procedures) regression — FIXED
`delegation.SessionDelegatorBaseImplTest` and 4 related classes
(`sql.storedproc.ResultMappingTest`, `sql.storedproc.StoredProcedureResultSetMappingTest`,
`sql.storedproc.StoredProcedureTest`, `jpa.procedure.StoredProcedureResultSetMappingTest`)
all failed with the exact original HIB-CV-27 symptom:
```
/org/h2/dynamic/FINDONEUSER.java:3: error: package org.h2.tools does not exist
```

**Root cause was NOT a `-classpath`/`;`-vs-`:` string-assembly bug** (the
original hypothesis) — H2's `SourceCompiler` never constructs a `-cp` string
at all; it relies on the JDK's real `Locations` default classpath decoding.
The actual bug: `java.io.File.<clinit>` derives
`separatorChar`/`separator`/`pathSeparatorChar`/`pathSeparator` by
constructing a real `UnixFileSystem`, whose constructor reads
`System.getProperties().getProperty("file.separator"/"path.separator")` —
against the REAL `Properties.map` (`ConcurrentHashMap`) field. CratonVM's
`System.getProperties()` singleton is allocated via `alloc_concurrent_synthetic`
(no real constructor runs), so `map` is `null`. `file.separator` happens to
get read first in `UnixFileSystem`'s constructor and NPEs shortly after on
`path.separator`; per JVMS §5.5 a swallowed `<clinit>` failure leaves whatever
static fields were already assigned intact (`separatorChar`/`separator`,
correct) while the rest stay at their zero-init default — so
`File.pathSeparatorChar` was silently `'\0'` and `File.pathSeparator` was
`""` on Linux (confirmed via probe: `System.getProperty("path.separator")`
correctly returned `:`, but `File.pathSeparator` printed empty). Real javac's
`com.sun.tools.javac.file.Locations` decodes the default `-classpath` from
`File.pathSeparator`-delimited `java.class.path`; with an empty separator the
entire 242-entry classpath string (including `h2-2.4.240.jar`) collapsed into
one bogus unsplit "path" that obviously doesn't resolve to any real file —
`Files.readAttributes` throws, the JAR's container is treated as
`MISSING_CONTAINER` (silently — no exception), and package lookups inside it
report "package X does not exist" for every package, including
`org.h2.tools`.

**Fix**: `../../../../vm/src/vm/vm_util.rs` — added a `"java/io/File"` arm to the existing
`post_clinit_fixup` mechanism (same established pattern already used for
`UnsafeConstants`/`BigInteger`/`PosixFilePermission`), unconditionally
backfilling all four separator fields from `std::path::MAIN_SEPARATOR` /
`cfg!(windows)`. These are pure platform constants (not user-configurable),
so an unconditional overwrite is always correct — this does not touch or
attempt to fix the underlying `Properties`/`ConcurrentHashMap` bootstrap gap
(a much larger, riskier change), it just makes File's four derived constants
deterministically correct regardless of whether the upstream Properties
plumbing NPEs.

**Verification**: all 3 Bug-A classes + 4 Bug-B classes pass on Linux
(Azure host, `--java-home /home/victor/jdk25`, real classpath, no `--nojit`
flag needed):
```
DriverManagerRegistrationTest                        ok=2 failed=0
DialectFactoryTest                                   ok=6 failed=0
DialectSpecificConfigTest                            ok=6 failed=0
SessionDelegatorBaseImplTest                         ok=1 failed=0
sql.storedproc.ResultMappingTest                      ok=4 failed=0
sql.storedproc.StoredProcedureResultSetMappingTest    ok=1 failed=0
sql.storedproc.StoredProcedureTest                    ok=4 failed=0
jpa.procedure.StoredProcedureResultSetMappingTest     ok=1 failed=0
```
A 50-class regression slice (`others.txt`, previously-passing classes
spanning annotations/CDI/classloading/etc.) was also re-run against the fixed
binary to check for fallout from the `File` fixup (it's a very hot bootstrap
path touched by nearly everything) — see the run log for the pass/fail count.

Both fixes are platform-neutral by construction (verified by code inspection,
not by an actual Windows build+run in this session — `LinuxSocketOptions` is
inert on Windows since the class doesn't exist there, and the `File` fixup
uses the same `cfg!(windows)` ternary as every other platform-conditional
constant in this codebase). **Merged to `dev`** via commit `81a31c08`
("Merge fix/linux-jdbc-serviceloader-and-javac-cp into dev") — confirmed
present in both `../../../../native-io/src/net.rs` and `../../../../vm/src/vm/vm_util.rs` on current
dev (2026-07-06 verification).

## Fixed 2026-07-06 — re-triage of section C (all 10 classes)

Re-ran all 10 classes below against current dev (Windows box, `9f1db39d`+,
real-JDK, JIT on) to re-verify before investing more root-causing effort.
**All 10 now pass.** Breakdown:

- **8 already fixed by unrelated earlier work this week** (the triage doc
  simply hadn't been re-checked against a fresh binary):
  - `mapping.type.java.JdbcTimestampJavaTypeTest` — passes (4/4).
  - `mapping.mutability.attribute.BasicAttributeMutabilityTests` — passes (7/7).
  - `softdelete.SoftDeleteFetchModeTests` — fixed by the `ConcurrentHashMap`
    default-capacity/iteration-order fix; see
    `../hib-softdeletefetchmodetests-chm-order-RESOLVED.md`.
  - `mapping.fetch.depth.NoDepthTests` — the JPA-variant `PersistenceProvider`
    ServiceLoader residual is gone (no dedicated fix needed — closed by
    unrelated classloader-resource-resolution hardening); see
    `docs/internal/hibernate-bugs/hib-nodepthtests-persistenceprovider-serviceloader-residual.md`.
  - `engine.spi.EntityEntryTest` — passes (5/5); the previously-reported
    `MockitoException` no longer reproduces.
  - `timezones.JDBCTimeZoneZonedTest`, `timezones.PassThruZonedTest`,
    `timezones.UTCNormalizedInstantTest` — fixed by the `TimeZone`
    standard-offset fix (HIB-CV-34); see
    `docs/internal/hibernate-bugs/run-20260622/HIB-CV-34-jdbc-timezone-timestamp-offset.md`.
- **1 genuinely new bug, root-caused and fixed this session:**
  `serialization.CacheKeyEmbeddedIdEnanchedTest` —
  `InvalidClassException: local class incompatible: stream classdesc
  serialVersionUID mismatch`. The original hypothesis ("CratonVM computes a
  different implicit serialVersionUID than HotSpot") was **wrong** — both
  sides compute the SAME algorithm; the actual bug is that deserialization
  resolved the stream's class name through the **wrong classloader**,
  silently loading a *different*, structurally-different same-named class
  (missing Hibernate's bytecode-enhancement transform). Three separate bugs
  contributed, all in the same family (class-name resolution that ignores
  which classloader the deserializing caller actually expects):
  1. `jdk/internal/misc/VM.latestUserDefinedLoader0()` was a hardcoded stub
     always returning `null`, breaking `ObjectInputStream.resolveClass()`'s
     default `Class.forName(name, false, latestUserDefinedLoader())` — fixed
     by implementing a real stack walk (mirroring HotSpot's
     `JVM_LatestUserDefinedLoader`).
  2. The synthetic deserialization fast path (`ois_read_object` in
     `../../../../native-builtins/src/serialization.rs`) resolved class names via the
     loader-oblivious `ensure_class_initialized`, bypassing `resolveClass`
     entirely — fixed to consult the same stack-walk-derived loader first.
  3. `Constructor.newInstance`'s "serialization constructor" special case
     (`ReflectionFactory.newConstructorForSerialization` /
     `DirectConstructorHandleAccessor`, used for any Serializable class
     whose nearest non-Serializable ancestor is `Object`) allocated the new
     instance by class **name** instead of by the already-resolved
     `ClassId` — silently collapsing to whichever same-named class the
     process registered first. Fixed to allocate via the resolved `ClassId`
     directly.

  A subtlety caught during verification: the stack-walk fix for (1)/(2)
  initially only worked when this test ran *first* in a batch. Re-resolving
  each stack frame's class by **name** (the only thing `capture_stack_trace`
  exposes) collapses to the first-ever-registered class of that name once a
  *second* `@BytecodeEnhanced` test class (each gets its own fresh
  `EnhancingClassLoader`) has run earlier in the same process. Fixed
  properly by adding `NativeContext::frame_class_ids()`, which exposes each
  live frame's precise, already-resolved `ClassId` (no name round-trip).
  Verified with all 10 classes above running together in one process, in
  original order, all passing.

  See `../../../../native-builtins/src/serialization.rs` (`latest_user_defined_loader_class`,
  `ois_read_object`), `../../../../native-builtins/src/lib.rs`
  (`VM.latestUserDefinedLoader0`), `../../../../native-builtins/src/lang_class.rs`
  (`native_constructor_new_instance`), and `../../../../native-api/src/registry.rs` /
  `../../../../vm/src/vm/vm_exec.rs` (`NativeContext::frame_class_ids`).
- **1 was never actually broken:** `boot.database.metadata.MetadataAccessTests`
  — the original `ServiceException` symptom no longer reproduces at all; a
  later re-run instead showed it not completing within a tight batch
  timeout, which looked like a hang but was purely a **cumulative slowdown
  artifact** of running many classes (each opening/closing real H2
  connections) in one process — the class reliably passes (32/32) both
  alone and as part of the full 10-class batch given a timeout sized for the
  batch's actual (slower-than-any-single-class) cumulative runtime. Not a
  CratonVM bug.

## Ruled out / already tracked (no new action)
- **7 classes shuffling between two already-broken statuses** (FAIL↔HANG↔CRASH in OSR on/off) — see `../jit-osr-linux-regression-triad.md`.
- **`proxy.ProxyClassReuseTest`** (`ClassCastException`) — already tracked in `docs/known-issues/hib-proxyclassreuse-loader-blind-class-resolution.md`.
- **`service.ClassLoaderServiceImplTest`** — already tracked (HIB-CV-24 area).
- **9 "Could not build SessionFactory: To-one map..." classes** (`EnhancedProxyCacheTest`, `AutoFlushBeforeLoadTest`, `JoinFetchWithEnhancementTest`, `PrivateConstructorEnhancerTest`, `LockExistingBytecodeProxyTest`, `OneToOneEmbeddedIdTest`, `OneToOneJoinColumnsEmbeddedIdTest`, `DetachedEntityParameterAutoFlushVersionTest`, `BaseIdEntityByteCodeTest`) — enhancement-bootstrap-adjacent, same root cause as the tracked bytecode-enhancement cluster.
- **`query.CachedQueryShallowWithDiscriminatorBytecodeEnhancedTest`** — name says bytecode-enhanced, same cluster.
- **Timeout-classified (known slowness, not new)**: `batch.BatchTest`, `batchfetch.DynamicBatchFetchTest`, `hql.HqlParserMemoryUsageTest`, `id.enhanced.OptimizerConcurrencyUnitTest`, `id.uuid.rfc9562.UUidV6V7GeneratorTest`, `jpa.lock.LockTest`, `sql.exec.SmokeTests` — all part of the known throughput-wall slowness cluster identified earlier this session (need >600s or hit JUnit's internal 120s/method limit, not correctness bugs).
