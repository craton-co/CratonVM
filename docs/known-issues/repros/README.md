# Repro classes for the remaining (open) known-issue bugs

Easy-access, tracked copies of the standalone reproducers for the **open** bugs in
`docs/known-issues/`. Previously these lived only in gitignored locations
(`wildfly-suite/`, `spring-suite/`, `scratch/`, `apps/*/.cratonvm-suite/`) and were
at risk of being lost. The bugs span **Wildfly, Kafka, Hibernate, Elasticsearch,
Keycloak and Spring** — not Spring-only.

> **Re-run 2026-06-29 / archive cleanup 2026-07-01:** the **A2** (`ReflRepro`)
> and **A5** (`gc-stress` bintrees) repro sets were retired from this tracked directory
> (their bugs closed);
> three more — `spring-bug-08`,
> `keycloak-15`, `keycloak-16` — **no longer reproduce on dev** (see ✅ rows below). `A4`/`Fork6`
> still exercises the architectural FJP register-root gap (non-fatal: prints `ALL-OK`).

Run pattern (replace `$CV` with a built binary, `$JDK` with the JDK 25 home):
```
javac <Repro>.java
"$JDK\bin\java.exe" -cp <dir> <Repro>     # HotSpot baseline (prints RESULT=OK)
$CV --java-home "$JDK" -cp <dir> <Repro>  # CratonVM (reproduces the gap)
```

## Standalone, pure-JDK (no app classpath) — `javac` + run directly

| Bug doc | Repro here | How to trigger / expected |
|---|---|---|
| `fork6-…` (**A4**) | `A4-fork6/Fork6.java` | `CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 $CV … Fork6` → NPE/CCE in workers (~rep 4). HotSpot/`--nojit`/`-Xmx8g` print `ALL-OK`. |
| `spring-bug-08-…` | `spring-bug-08-proxy-serialization/ProxySer.java` | ✅ **NOW PASSES on dev** (re-run 2026-06-29: `RESULT=OK`, no `UnsatisfiedLinkError`). Was: serialize→deserialize a `Serializable` JDK proxy → `UnsatisfiedLinkError: Module.defineModule0` on deserialize. |
| `keycloak-15-…` | `keycloak-15-path-root/PathRoot.java` | ✅ **NOW PASSES on dev** (re-run 2026-06-29: `getRoot()=C:\`, `nameCount=2`, ==HotSpot). Was: `Paths.get("C:\\foo\\bar")` → `getRoot()=null`, `nameCount=3`. |
| `keycloak-16-…` | `keycloak-16-stream-onclose/StreamOnClose.java` | ✅ **NOW PASSES on dev** (re-run 2026-06-29: onClose ran, lazy `peek=1`, ==HotSpot). Was: `onClose` handler dropped (close() no-op) and eager `peek` (`peeked=5`). |
| `bug06-fam5-…` | ✅ **CLOSED 2026-07-02** — repro retired (bug fixed) | failcause extinct: 0 × `getDeclaredMethod on null` across the clean 2026-06-30/07-01 full re-runs and a fresh 196-class nojit+jit sweep on dev `ffb247e5`; probe stays ==HotSpot. |
| `springrepos-…` (latent deep recursion) | `springrepos-deep-recursion/GroovyNestProbe.java` | deeply-nested Groovy closures; clean `dev` runs slow-not-crash — the native-stack overflow only with the unmerged cold-path JIT experiment. |
| `g1-parallel-evac-persistent-forwarding-root-remap` | `g1-parallel-steady-churn/SteadyChurn.java` | fixed 2026-07-04; the tracked recreation here and `run-g1-parallel-steady-churn.ps1` are retained as regression assets. |
| `jetty-webserver-factory-poststartup-timeout-…` (TLD/JAR-scan residual) | `xerces-sax-manysmallfiles-slowdown/SaxManySmallFiles.java` + `SaxEncodingCompare.java` | Reused-`SAXParser` repeated small-doc parse — no file/jar/classpath I/O. HotSpot ~40-100us/parse; CratonVM ~8-13ms/parse (100-200x). `SaxEncodingCompare` refutes the `UTF8Reader`-decoder hypothesis (ASCII was not faster). Seconds per iteration instead of the full suite's 10+ minute rebuild-and-rerun cycle. |

## GC-root-race / Family-A probe (needs `GC_STRESS` or load)

| Bug doc | Repro here | Note |
|---|---|---|
| `spring-bug-10-…`, `springsuite-bug-04-…` | `family-a-gc-root-race/MTRegex.java` | multi-thread regex GC-root-undercount probe. Single-thread register-invisibility is closed by precise-maps default-on; this probe exercises the multithread residual. |

## JTA / Narayana (needs the Hibernate suite classpath — `@common.args`)

| Bug doc | Repro here | Note |
|---|---|---|
| `hibernate-jta-narayana-…` | `jta-narayana-xa/XaProbe.java` | decisive: prints which `XAResource` callback fires (enlist/commit/prepare) — pins Layer-1. Needs `narayana-jta` on cp + `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1`. |
| | `jta-narayana-xa/TxCtl.java` | Layer-0 standalone: `Class.forName("…TxControl")` (entry crash, now fixed). |
| | `jta-narayana-xa/SSRepro.java` | `new ServerSocket(0,50,localhost).getInetAddress()` (Layer-0, now fixed). |

## No portable standalone repro — reproduce via the app test class

These need the app's test classpath (KRun / JUnitCore + the suite jars); see each doc's
"Reproduce" section for the exact command. Listed here so the access path is in one place.

| Bug doc | Test class / entry |
|---|---|
| `spring-bug-04-…` (`@Timeout`) | `org.springframework.aot.generate.ValueCodeGeneratorTests` (KRun, spring-core testcp) |
| `spring-bug-06-…` | `org.springframework.core.annotation.MergedAnnotationsTests` |
| `spring-bug-11-…` (Groovy hang residual) | `org.springframework.scripting.groovy.GroovyScriptEvaluatorTests` |
| `bug06-fam6-…` (2 GB OOM) | `org.springframework.core.annotation.AnnotationUtilsTests` (small `-Xmx` to fail fast) |
| `kafka-bug-B-mockstatic-capturing-lambda-jit` | `ksuite/repro/MockClientUtils.java` (Kafka clients + Mockito cp) |
| `keycloak-credentialmodel-jit-…` | keycloak `…CredentialModel.getAdditionalParameters()` path (kcfull report 18). **Standalone repros `keycloak-credentialmodel-jit/{LazyNull,EaRepro}.java` are negative controls** — they show the bare lazy-init shape is compiled correctly by both backends (escape-analysis hypothesis refuted 2026-06-21; needs the real Jackson-deser caller context). |
| `hibernate-jaxb-classloading-bytebuddy-bootstrap-slow` | `org.hibernate.orm.test.hql.HQLTest` (Hibernate suite, `@common.args`) |
| `hibernate-deserialization-sessionfactory-reconnect-null` | `…jpa.serialization.EntityManagerDeserializationTest` |
| `reactor-worker-thread-leak-at-shutdown` | `org.elasticsearch.client.RestClient*IntegTests` (intermittent) |
| `ES-HANG-02` residual-2 (throughput) | `org.elasticsearch.client.RestClientSingleHostIntegTests.testManyAsyncRequests` |
