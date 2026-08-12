# App readiness under `--jdk-only` — what stops a real Java application today

**Measured 2026-08-12 on this Windows host.** Everything below was RUN. No row
in this document is inferred from source reading alone; where source is quoted
it is to name a mechanism whose *effect* was measured first.

* VM under test: `cratonvm-f8.exe` (current, all campaign fixes).
* Control: `cratonvm-control-44044c7e2.exe` (pristine dev) — run on the JDBC
  workload only, to separate "pre-existing" from "this session broke it".
* Oracle: Microsoft OpenJDK **25.0.3+9-LTS**, `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`.
* Three arms everywhere: `hs` (HotSpot), `cvjdkonly` (`--jdk-only`),
  `cvdefault` (CratonVM's default Compatible mode). **The third arm is what
  makes an attribution possible**: a failure present in `cvjdkonly` and absent
  in `cvdefault` is a strict-mode gap; a failure in both is a VM defect; a
  failure in `hs` too is my harness or a genuine upstream difference.

## 0. The one-paragraph answer

**Real applications DO run under `--jdk-only` on this host.** Embedded Tomcat
12 boots, serves HTTP GET/POST and a 404, and shuts down cleanly under
`--jdk-only` — measured twice. A 27-point sweep of the JDK surface a real app
actually touches passes 26/27. Reflection, annotations, dynamic proxies,
`MethodHandles`, `URLClassLoader`, `ServiceLoader`, the whole
executor/lock/`CompletableFuture` stack, files, NIO, sockets, `HttpURLConnection`,
`java.net.http.HttpClient`, JCA, XML — all green.

**Three defects stop everything else, and all three are the same mechanism.**
A native that survives strict-mode registration allocates a *fabricated* class
that strict mode refuses, so the call site dies with `NoClassDefFoundError`
naming a `cratonvm/…` or `…$RustJvmImpl` type. Measured instances:

1. `Atomic{Reference,Integer,Long}FieldUpdater.newUpdater` → **`java.sql` is
   unloadable**, so no JDBC, no ORM, no connection pool, no datasource.
2. `System.getenv()` (no-arg) → **Spring cannot create an `Environment`**, so
   no Spring context, and by extension no Spring Boot.
3. `SSLSocket.getOutputStream()`/`getInputStream()` → **no TLS**, so no HTTPS
   client or server.

Fixing those three unblocks, on this evidence, every application shape I could
build a workload for. That is a small, well-localised, parallelisable list —
which is the most important thing this lane has to report.

---

## 1. What real workloads exist, and which are runnable HERE

`apps/` is gitignored repo-wide, so **none of the corpora are in this
worktree**. They live in the main checkout and in a sibling tree, and both are
readable from here:

| corpus | location | built on this host? | runnable here |
|---|---|---|---|
| **H2 Database** | `C:\craton\cratonvm\apps\h2database\h2\target\{classes,test-classes}` | **yes** — 1801 `.class` files, 218-class suite baseline in `apps/h2database-suite-runner/baseline.tsv` | **yes**, directly: every `org.h2.test.*` class is its own `main()` |
| **Tomcat** | `C:\craton\apps\tomcat\output\{classes,testclasses,build\lib\*.jar,build\bin\*.jar}` | **yes** — full binary layout, 34 jars | **yes**, embedded `org.apache.catalina.startup.Tomcat` |
| **Spring Framework 7.1** | `C:\craton\cratonvm\apps\spring-framework\<module>\build\classes\java\main` | **yes**, per module | **yes** — `AnnotationConfigApplicationContext` |
| Hibernate ORM | `C:\craton\apps\hibernate-orm` | yes (438 jars) | plausible, not attempted (needs JDBC → blocked by family A anyway) |
| Keycloak | `C:\craton\apps\keycloak`, `C:\craton\cratonvm\apps\keycloak` | yes (500+ jars) | needs its runner; JDBC + TLS → blocked by A and C |
| WildFly | `C:\craton\apps\wildfly` | yes (198 jars) | needs its runner |
| Elasticsearch | `C:\craton\apps\elasticsearch` | yes (59 jars + classes) | needs its runner |
| Kafka | `C:\craton\cratonvm\apps\kafka` | yes (255 jars) | needs its runner |
| commons-math, bc-java, dacapobench | `apps/…` both trees | yes | not attempted |
| **Spring Boot** | `apps/spring-boot` holds only `sb-runner` | **no checkout** | **NOT runnable here** — the Spring Boot corpus is absent from this host |

**Correction to a common assumption: the corpus is NOT the constraint on this
host.** Every suite except Spring Boot has compiled bytecode sitting on local
disk. What is missing is per-suite *runner setup* (the H2 runner is bash and
wants `mvn dependency:build-classpath`; the Tomcat runner wants a ~20 min `ant`
recompile), not the code. I bypassed both runners by composing the classpath by
hand from the already-built output, which is why this lane could measure real
applications in one session. See §7 for what that does and does not buy.

In-tree runners, for the record: `apps/h2database-suite-runner/run-h2-suite.sh`
(Linux-targeted), `apps/tomcat-suite-runner/run-tomcat-suite.ps1`,
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`,
`apps/{hib,elasticsearch,keycloak,wildfly,spring}-suite-runner/`,
`test-infra/run-all-apps-suites.sh`.

## 2. Harness — the exact commands

Everything went through one script (kept in scratch, not in the tree), which is
just three fixed command lines plus `timeout` and per-arm output capture:

```sh
# arm hs
timeout -k 5 $TMO "C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot\bin\java.exe" \
        $EXTRA -cp "$CP" "$CLS"
# arm cvjdkonly
timeout -k 5 $TMO "$BIN\cratonvm-f8.exe" --jdk-only $EXTRA -c "$CP" "$CLS"
# arm cvdefault
timeout -k 5 $TMO "$BIN\cratonvm-f8.exe"            $EXTRA -c "$CP" "$CLS"
# arm ctljdkonly
timeout -k 5 $TMO "$BIN\cratonvm-control-44044c7e2.exe" --jdk-only $EXTRA -c "$CP" "$CLS"
```

Classpaths used:

```text
H2       C:\craton\cratonvm\apps\h2database\h2\target\classes;…\target\test-classes
Tomcat   <probe classes>;C:/craton/apps/tomcat/output/build/lib/*.jar;…/build/bin/*.jar
Spring   <probe classes>;C:/craton/cratonvm/apps/spring-framework/{spring-core,
         spring-beans,spring-context,spring-aop,spring-expression,spring-tx,
         spring-jdbc}/build/{classes/java/main,resources/main};
         …/spring-core/build/libs/spring-objenesis-repack-3.5.jar;
         commons-logging-1.3.5.jar (gradle cache);
         C:/craton/cratonvm/apps/h2database/h2/target/classes
```

Two harness facts worth carrying forward:

* **`-D` works** on `cratonvm` even though `--help` does not list it
  (`-Dfoo=bar` round-trips through `System.getProperty`). Verified against
  HotSpot with the same one-liner.
* **`--jdk-only-report <FILE>` needs a Windows-shaped path.** Given a
  `/c/Users/...` path it prints `could not write JDK-only report … (os error 3)`
  and continues, and the file silently does not appear. It is also **not
  written when the program calls `System.exit`** — every probe here grew a
  `-Dprobe.noexit=1` escape so the census could be taken.

## 3. Results

### 3.1 The two-line witness

`SqlWitness.java` — the whole of family A and family B in one screen. Run twice
per arm, identical both times:

```text
HotSpot          OK   java.sql.SQLException initialised
                 OK   System.getenv() size=87
cratonvm --jdk-only
                 FAIL java.sql.SQLException :: java.lang.NoClassDefFoundError:
                      java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl
                 FAIL System.getenv() :: java.lang.NoClassDefFoundError:
                      cratonvm/internal/UnmodifiableMap
```

### 3.2 Workload matrix

| workload | HotSpot | `--jdk-only` | `cvdefault` | verdict |
|---|---|---|---|---|
| **Embedded Tomcat 12** — boot, `GET /hello`, `POST /hello`, `GET /nope`→404, `stop()+destroy()` | 4/4, 3.9 s | **4/4 twice**, 27.9 s and 10.6 s | 4/4, 22.1 s | **PASS in strict mode.** Startup 3–7× HotSpot; the two strict runs differ by 2.6× purely on host load, which is the whole argument of §7's timing caveat |
| **`SurfaceProbe`** — 27 JDK surfaces a real app calls | 27/27 | **26/27** | 27/27 | one failure: `System.getenv()` |
| **`CoreProbe`** — 14 vectors aimed at the fabricated-class producers the census names | 14/14 | **11/14** | 14/14 | the three reds are the three field updaters; every collection vector green |
| **`ReflectProbe`** — 16 class-loading / reflection / annotation / proxy vectors | 16/16 | **16/16** | — | PASS |
| **`ServiceProbe`** — 11 `ServiceLoader` / module / resource vectors | 11/11 | **11/11** | — | PASS (one soft divergence, §3.5) |
| **`ConcurrencyProbe`** — 17 thread / executor / lock / CF vectors | 17/17 | **17/17** | — | PASS |
| **`IoNetProbe`** — 15 file / NIO / socket / HTTP / TLS vectors | 15/15 | **13/15** | 14/15 | TLS streams dead in strict; async-close bug in Compatible |
| **`JdbcProbe`** — 10 real-H2 JDBC vectors | 10/10 | **1/10** | 10/10 | control binary also 1/10 → pre-existing |
| **`SpringProbe`** — Spring 7.1 DI / SpEL / AOP / `JdbcTemplate` | 8/8 | **0/8** | — | dies before the first bean |
| **H2 suite, JDBC classes** (`TestAlter`, `TestCsv`, `TestPreparedStatement`, `TestMVStore`) | 3/4 (`TestMVStore` red on HotSpot too) | **0/4** | 2/4 (2 timeouts) | all four die on family A |
| **H2 suite, `unit` classes** (8 classes) | 8/8 | **4 pass / 2 fail / 2 over budget** | — | both reds are family A; §3.4 |

### 3.3 First-failure stacks

**Family A — `java.sql` is unloadable.** `java.sql.SQLException` (JDK 25 source
line 373) holds
`private static final AtomicReferenceFieldUpdater<SQLException,SQLException> nextUpdater`.
Its `<clinit>` therefore runs `newUpdater`, which CratonVM intercepts with a
native that allocates a fabricated impl class, which strict mode refuses:

```text
Exception in thread "main" java/lang/NoClassDefFoundError:
    java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl
  at java/sql/SQLException.<clinit>(SQLException.java:374)
  at org/h2/message/DbException.<clinit>(DbException.java:62)
  at org/h2/message/TraceObject.<clinit>(TraceObject.java:108)
  at org/h2/Driver.connect(Driver.java:59)
  at java/sql/DriverManager.getConnection(DriverManager.java:613)
  at java/sql/DriverManager.getConnection(DriverManager.java:199)
```

(frames innermost-last, as CratonVM prints them). The same family, different
class, on the MVStore path — this one is `AtomicIntegerFieldUpdater`, and it is
reached without any JDBC at all:

```text
Exception in thread "main" java/lang/NoClassDefFoundError:
    java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl
  at org/h2/mvstore/MVStore$TxCounter.<clinit>(MVStore.java:1919)
  at org/h2/mvstore/MVStore.<init>(MVStore.java:224)
  at org/h2/mvstore/MVStore$Builder.open(MVStore.java:2193)
```

After the first failure the app class is left erroneous, so every later attempt
reports `NoClassDefFoundError: org/h2/jdbc/JdbcConnection` — that secondary
shape is correct JVMS behaviour and is **not** a second defect.

**Family B — Spring cannot build an `Environment`.**

```text
FAIL AnnotationConfigApplicationContext refresh ::
     java.lang.NoClassDefFoundError: cratonvm/internal/UnmodifiableMap
  at org.springframework.core.env.AbstractEnvironment.getSystemEnvironment(AbstractEnvironment.java:450)
  at org.springframework.core.env.StandardEnvironment.customizePropertySources(StandardEnvironment.java:100)
  at org.springframework.core.env.AbstractEnvironment.<init>(AbstractEnvironment.java:138)
  at org.springframework.context.support.AbstractApplicationContext.createEnvironment(AbstractApplicationContext.java:351)
```

**Family C — TLS has no streams.** The handshake *succeeds* — the probe printed
`cipher=TLS_AES_128_GCM_SHA256 proto=TLSv1.3` from `getSession()` — and then:

```text
FAIL TLS handshake + echo (SSLServerSocket) ::
     java.lang.NoClassDefFoundError: javax/net/ssl/SSLSocketOutputStream
  at IoNetProbe.lambda$main$15(IoNetProbe.java:214)
```

### 3.4 H2 `unit` classes — a correctness red plus a throughput wall

`org.h2.test.unit.TestStringUtils` and `TestSecurity` **fail under `--jdk-only`
and pass on HotSpot**. Both are family A. `TestStringUtils` is the more
interesting of the two because it never touches JDBC: the cause reaches it
through `DbException` (H2's exception type extends `SQLException`), which turns
an expected `DbException` into a `NoClassDefFoundError` — i.e. family A
corrupts *exception type identity*, not just connection setup:

```text
java/lang/AssertionError: Expected an exception of type
DbException to be thrown, but an exception of type
NoClassDefFoundError was thrown
  at org/h2/test/unit/TestStringUtils.testHex(TestStringUtils.java:84)
Caused by: java/lang/NoClassDefFoundError:
    java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl
  at java/sql/SQLException.<clinit>(SQLException.java:374)
```

The rest of the `unit` sample is a **throughput** story, not a correctness one.
`TestBitStream` timed out at 120 s, was re-run at 420 s and **passed** — so a
`124` in this table means "slower than the budget", and calling it a hang
without a second, longer run would have been wrong. Wall times, single run each,
on a loaded shared host (treat as orders of magnitude, not measurements):

| class | HotSpot | `--jdk-only` |
|---|---|---|
| `TestIntArray` | 6 s pass | 14 s pass |
| `TestMathUtils` | 3 s pass | 3 s pass |
| `TestPattern` | 2 s pass | 8 s pass |
| `TestScriptReader` | 2 s pass | 3 s pass |
| `TestBitStream` | fast pass | pass, but only above the 120 s budget (420 s re-run) |
| `TestStringUtils` | 3 s pass | **FAIL** — family A |
| `TestSecurity` | 5 s pass | **FAIL** — family A (`DriverManager.getConnection`) |
| `TestSort` | 6 s pass | **exceeded 480 s** |
| `TestJsonUtils` | 6 s pass | **exceeded 480 s** |
| `TestDateTimeUtils`, `TestCompress` | pass | exceeded 120 s (not re-run longer) |
| Tomcat boot | 3.9 s | 27.9 s / 10.6 s (two runs) |

Tally for the eight `unit` classes run in both arms: HotSpot 8/8;
`--jdk-only` **4 pass, 2 fail on family A, 2 over a 480 s budget**. The two
budget rows are explicitly *not* counted as failures — `TestBitStream` is the
proof that they might not be.

`TestGeometryUtils` fails on **HotSpot** too and is excluded from every count —
that is an upstream/fixture difference, not a VM defect.

### 3.5 Divergences that are not failures

* **`java.util.logging` routing.** `Logger.info` reaches **stderr** on HotSpot
  and on `--jdk-only`, but **stdout** under `cvdefault`. Strict mode is the
  *correct* one here. Relevant to the handoff's warning that
  `System.out.println` routing changed on failure paths.
* **`ServiceLoader.load(CharsetProvider.class)`** yields 1 provider on HotSpot
  and **0** under `--jdk-only`. Both "pass" because nothing asserts a count —
  which is exactly the shape a-suite-that-only-asserts-the-positive-cannot-see-an-indiscriminate-impl warns about. Module-path service
  providers appear not to be discovered.
* **TLS async close reproduces in Compatible mode.** `IoNetProbe`'s
  "reader blocked in a TLS read, another thread calls `close()`" vector:
  HotSpot unblocks the reader with `java.net.SocketException`; `cvdefault`
  leaves it blocked and fails with
  `reader still blocked 10s after close() — async close not observed`. This is
  an independent, runnable reproduction of the four TLS stream sites the
  campaign already knows about (W2-2-blocked-reader-async-close-wakeup.md).
  Strict mode cannot even reach the vector, because of family C.

## 4. Failure families and their mechanism

There is **one mechanism** with three reachable instances. Naming it precisely
matters more than the instance list, because the instance list will grow the
moment a new workload is run.

> **THE MECHANISM.** `--jdk-only` refuses *class fabrication*. It does **not**
> un-register the *natives that depend on fabrication*. A native registered in
> the essential/shipping set therefore survives into strict mode, runs, asks for
> its fabricated receiver, gets a refusal, and converts it to a
> `NoClassDefFoundError` at the application's call site. The refusal is
> deliberate and diagnosable; the *survival of its caller* is the defect.

The premise this violates is written into the tree in the doc comment on
`vm/src/vm/vm_init.rs::ensure_bootstrap_compat_class` (anchor on the function
name, not a line band):

> *"Under `--jdk-only` a real `java.util.Collections`/`Enumeration`/`Comparator`
> is on the boot classpath and runs its own bytecode; these stand-ins exist for
> the synthetic collection shims, which strict mode does not register."*

That premise is **false for at least one of the thirteen**.
`cratonvm/internal/UnmodifiableMap` is not only used by synthetic collection
shims — `native-builtins/src/lang_system.rs::wrap_system_env_map` allocates it,
and its caller `System.getenv()` is registered unconditionally from
`register_essential_natives_with_shims`, the set strict mode keeps. This is the
a-guard-scoped-by-a-stated-premise-is-only-as-good-as-the-premise shape: the
guard is real, its stated justification is not. Re-derive the premise for the
other twelve before trusting them; my probes clear the paths I could reach, not
the claim.

| family | instances | mechanism | blast radius (measured) |
|---|---|---|---|
| **A. Field updaters** | `Atomic{Reference,Integer,Long}FieldUpdater$RustJvmImpl`, minted by `atomic_updater.rs::alloc_impl`, registered by `register_atomic_updater_natives` from **`register_essential_natives_with_shims`** (`native-builtins/src/lib.rs`) — which is exactly the set strict mode keeps. The second call site is in `register_synthetic_overrides` and is irrelevant here | native override of `newUpdater` returns a fabricated 4-slot impl instead of letting the JDK's own `…FieldUpdaterImpl` bytecode run | **`java.sql.SQLException.<clinit>` — i.e. the entire `java.sql` package.** All JDBC, ORM, pools, datasources. Plus H2 MVStore, plus 5 Spring source files, 1 Tomcat, 1 Elasticsearch |
| **B. `System.getenv()`** | `cratonvm/internal/UnmodifiableMap` | `native-builtins/src/lang_system.rs::wrap_system_env_map` fabricates the read-only wrapper because the JDK's `ProcessEnvironment` needs native interop the VM does not provide; `System.getenv` is registered from `register_essential_natives_with_shims` too | **every Spring context**, and any framework that reads the environment map. Note `System.getenv(String)` (single-arg) is **fine** |
| **C. TLS streams** | `javax/net/ssl/SSLSocketOutputStream` (`native-builtins/src/phases_late/ssl_security.rs:3518`), `SSLSocketInputStream` (`:3469`) | the whole `SSLSocket` I/O path is a native, rustls-backed implementation with fabricated stream receivers; the JDK's `SSLSocketImpl` is overridden, so there is nothing to fall back to | **all HTTPS**, client and server. Handshake succeeds, first `getOutputStream()` throws |

Explicitly **not** a family, though I looked: the other 24 fabricated classes
the census found across six real workloads (§5) are requested in Compatible
mode and refused in strict mode **without breaking anything I could reach** —
`ArrayListSubList`, `HashMap$KeyItr`, `TreeSet$Itr`, `LazyOp`,
`StreamChainCollector`, `SystemLogger`, `Comparator$Native`,
`Enumeration$Impl`, `IteratorEnumeration`, `cratonvm/synthetic/Process*`, and
the ten remaining `Unmodifiable*`. `CoreProbe` exercises all of the collection
ones directly (sub-lists, key/entry iterators, `unmodifiable{List,Map,Set,
SortedSet,NavigableSet}`, comparators via `TreeMap`/`PriorityQueue`/
`Arrays.sort`, enumerations) and scores **11/14 under `--jdk-only`, where the
only three reds are the three field updaters** — i.e. every collection vector
is green while its stand-in class stands refused. `SurfaceProbe` covers
`Runtime.exec` and PKCS12 `KeyStore` the same way. So for those 24 the refusals
are exactly what the design intends.

## 5. The census — the prospective blocker list, taken without a fix

`--jdk-only-report` works in **Compatible** mode. That means a workload too
broken to run under strict mode can still be made to enumerate every fabricated
class it *would* hit. Six real runs (Spring, Tomcat, H2 `TestAlter`, `JdbcProbe`,
`SurfaceProbe`, `IoNetProbe`), all green in Compatible mode, produced **27
distinct compatibility classes**, of which 13 come from the bootstrap block and
14 from named natives:

| requester | classes |
|---|---|
| `vm/src/vm/vm_init.rs:743` (boot block) | 11 × `cratonvm/internal/Unmodifiable*`, `java/util/Comparator$Native`, `java/util/Enumeration$Impl` |
| `native-builtins/src/atomic_updater.rs:248` | 3 × `Atomic*FieldUpdater$RustJvmImpl` |
| `native-builtins/src/phases_late/ssl_security.rs:3469,3518` | `SSLSocketInputStream`, `SSLSocketOutputStream` |
| `native-collections/src/lib.rs:5940,13623,18223,19161,43546` | `ArrayListSubList`, `HashMap$KeyItr`, `cratonvm/stream/LazyOp`, `StreamChainCollector`, `TreeSet$Itr` |
| `native-builtins/src/lib.rs:27093` | `cratonvm/internal/SystemLogger` |
| `native-builtins/src/keystore.rs:2666` | `java/util/IteratorEnumeration` |
| `native-io/src/process.rs:644,4178` | `cratonvm/synthetic/Process`, `ProcessPipeInputStream` |

Per-workload counter totals from the same reports (Compatible mode):

| workload | boot-image classes | app classes | bridge invocations | synthetic-stub invocations |
|---|---|---|---|---|
| Spring | 725 | 939 | 67 941 | 4 516 |
| Tomcat | 1284 | 396 | 564 369 | 5 247 |
| H2 `JdbcProbe` | 739 | 344 | 115 651 | 3 678 |
| `SurfaceProbe` | 942 | 2 | 11 938 | 1 332 |

**Use this technique for the remaining suites.** Hibernate, Keycloak, WildFly,
Kafka and Elasticsearch are all built on this host; running each once in
Compatible mode with `--jdk-only-report` costs one run and yields its complete
prospective strict-mode blocker list, without needing any of them to survive
strict mode first.

**Caveat on the census, and it is a real one.** The startup warnings enumerate
only the 13 bootstrap fabrications. The other 14 are minted lazily, at the
first native call that needs them — so *the stderr banner under-reports the
blocker set by half*, and a strict run that boots cleanly has proved nothing
about what its natives will do at call time.

## 6. What it would take, and what can proceed in parallel

All three families are **independent** — different crates, different files, no
shared call path. Three lanes can run concurrently. None of them touches the
JIT, the GC, or the collectors.

**Family A — field updaters.** Two candidate shapes, and the choice needs a
run, not an argument:
*(a)* do not register `register_atomic_updater_natives` under
`CompatibilityMode::JdkOnly`, and let the JDK's own
`AtomicReferenceFieldUpdaterImpl` bytecode run. It needs
`Unsafe.objectFieldOffset` + CAS on real layouts, which `AtomicReference`
already exercises, so this is plausibly free — but the native exists because
the JDK path performs a reflective access check against a `Field` mirror whose
private layout CratonVM does not reproduce (the module doc comment at the head
of `atomic_updater.rs` states exactly this), so *(a)* may simply resurrect the original
`ClassCastException`. **Measure it before choosing.**
*(b)* keep the native but return a **real** JDK object — build the updater by
calling the JDK's own factory, or by returning an instance of a real class the
strict policy admits. Higher effort, no premise to falsify.
The acceptance test is one line: `Class.forName("java.sql.SQLException")` under
`--jdk-only`, plus `JdbcProbe` going 1/10 → 10/10.

**Family B — `System.getenv()`.** The narrow fix is to stop fabricating the
wrapper and instead call the real `java.util.Collections.unmodifiableMap` on
the real-layout `HashMap` the native already builds — the backing map is
already correct (the native builds a real `java/util/HashMap` with real field
slots, and `wrap_system_env_map`'s own comment records that the JDK exposes it
as `Collections$UnmodifiableMap` with the backing in slot 0); only the wrapper
is fabricated. The wider fix is to
support `ProcessEnvironment`'s native interop and drop the override entirely.
Acceptance: `SurfaceProbe` 26/27 → 27/27 and `SpringProbe` 0/8 → 8/8. **Note
that Spring's own census lists all three `Atomic*FieldUpdater` classes, so
family B alone will move Spring's first failure, not necessarily make it
green** — expect to need A and B together for a Spring verdict.

**Family C — TLS streams.** The largest of the three and the one with a known
trap: the async-close fix is unsound as stated (socket readiness is not stream
readiness; `rustls`' `wants_read()` is false while decrypted plaintext is
buffered, so a readiness gate deadlocks ordinary HTTP-over-TLS). Strict mode
needs the stream receivers to be classes it admits, which is a smaller and
separable question from the async-close semantics. **Do the strict-mode
receiver work first** — it is a fabrication question, family-identical to A and
B — and treat async close as its own lane with its own reproduction, which
`IoNetProbe`'s "TLS async close of blocked reader" vector now provides.

**Cross-cutting, and cheap: close the census gap.** The bootstrap banner lists
13 refusals; 14 more are minted lazily. Emitting the same `warn!` at *every*
`try_ensure_synthetic_class` refusal, not just the boot block, would have turned
all three families into a startup-visible list instead of three separate
application crashes. This is a one-lane change and it makes every later suite
run self-diagnosing.

**Not blocking, but measured here:** Tomcat boot is 3–7× HotSpot, and
individual H2 `unit` classes run from 1× to over 80× slower — two of eight
exceeded 480 s against a 6 s HotSpot time. On a real CI that is
indistinguishable from a hang, and this lane already caught itself calling one
such row a hang before a longer re-run passed it. The H2 baseline's `HANG` rows
deserve the same treatment before any of them is treated as a defect.

## 7. What this evidence does and does not license

**It licenses:**

* That `--jdk-only` runs a real servlet container end to end. Tomcat 12 boot →
  request → response → shutdown, twice, no fabrication failures.
* That the three families above are **real, reproduced at least twice each, and
  pre-existing** — the pristine `44044c7e2` control fails `JdbcProbe`
  identically (1/10, same first stack), so none of this is campaign damage.
* That the *mechanism* is a single one, and that the 24 other fabricated
  classes are, on every path I could reach, correctly refused without harm.
* That reflection, annotations, proxies, class loading, `ServiceLoader`,
  concurrency, file/NIO/socket I/O and plain HTTP are not what is blocking
  applications.

**It does NOT license:**

* **Any claim about a *second* failure.** Every red here is a **first**
  failure. Spring is 0/8 because it dies before bean one; H2's JDBC classes are
  0/4 for the same reason. What lies behind family A and B is **unmeasured**,
  and the census says at minimum that Spring will meet family A after family B
  is fixed. Do not read "three families" as "three fixes to green".
* **Any claim about Spring Boot, Kafka, Keycloak, WildFly, Elasticsearch or
  Hibernate.** I ran none of them. Spring Boot has no checkout on this host at
  all. The others are built and could be run, but were not.
* **Any throughput conclusion.** Wall times were taken on a shared host under
  concurrent agent load, single run each. They are order-of-magnitude only —
  see shared-host-wall-time-multithread-measurements-are-worthless. The one
  quantitative claim I stand behind is qualitative: a 120 s budget is too small,
  because a class that "timed out" passed at 420 s.
* **Anything about `--synthetic-jdk` mode**, which this lane did not run, or
  about the Linux arms of `native-io/src/process.rs`, which this host cannot
  compile.
* **Any claim that the probe suite is complete.** It is 100-odd assertions
  written in one session against the shapes I judged representative. A green
  probe means the vector I wrote passes, not that the surface is correct — and
  §3.5 shows two divergences that only surfaced because I *printed* a value
  rather than asserting it.

**One methodological warning for whoever picks this up.** My first TLS result
was wrong in both directions: the HotSpot arm failed (my server never completed
the handshake) and the `--jdk-only` arm reported the async-close vector as
**OK** — because it had already died with `NoClassDefFoundError`, which the
vector counted as "the reader unblocked". A probe's setup is code and can be
wrong, and a fast failure can masquerade as a pass. Both arms had to be fixed
and re-run before either number meant anything.
