# Archived repro classes from fixed and investigated known issues

Easy-access, tracked copies of standalone reproducers archived from
`docs/known-issues/` and prior investigation notes. Previously these lived only in gitignored locations
(`wildfly-suite/`, `spring-suite/`, `scratch/`, `apps/*/.cratonvm-suite/`) and were
at risk of being lost. The bugs span **Wildfly, Kafka, Hibernate, Elasticsearch,
Keycloak and Spring** — not Spring-only.

Run pattern (replace `$CV` with a built binary, `$JDK` with the JDK 25 home):
```
javac <Repro>.java
"$JDK\bin\java.exe" -cp <dir> <Repro>     # HotSpot baseline (prints RESULT=OK)
$CV --java-home "$JDK" -cp <dir> <Repro>  # CratonVM (reproduces the gap)
```

## Standalone, pure-JDK (no app classpath) — `javac` + run directly

| Bug doc | Repro here | How to trigger / expected |
|---|---|---|
| `reflrepro-…` (**A2**, fixed) | `A2-reflrepro/ReflRepro.java` | ✅ **NOW PASSES on dev** (re-run 2026-06-29: `CRATONVM_DBG_GC_STRESS=65536 $CV … -cp A2-reflrepro ReflRepro 8000` → `ok=8000 bad=0 rc=0`). Was: rc=139 UAF from GC free-list double-serve. |
| `fork6-…` (**A4**) | `A4-fork6/Fork6.java` | `CRATONVM_REAL_FORKJOINPOOL=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 $CV … Fork6` → NPE/CCE in workers (~rep 4). HotSpot/`--nojit`/`-Xmx8g` print `ALL-OK`. |
| `spring-bug-08-…` | `spring-bug-08-proxy-serialization/ProxySer.java` | serialize→deserialize a `Serializable` JDK proxy → **`UnsatisfiedLinkError: Module.defineModule0`** on deserialize. HotSpot `RESULT=OK`. |
| `keycloak-15-…` | `keycloak-15-path-root/PathRoot.java` | `Paths.get("C:\\foo\\bar")` → `getRoot()=null`, `nameCount=3` (HotSpot `C:\`, 2). |
| `keycloak-16-…` | `keycloak-16-stream-onclose/StreamOnClose.java` | `onClose` handler dropped (close() no-op) **and** eager `peek` (`peeked=5` vs lazy 1). |
| `bug06-fam5-…` | `bug06-fam5-reflection-null/Refl5.java` | ✅ **CLOSED 2026-07-02** — probe ==HotSpot in nojit+jit on dev `ffb247e5`; the suite-level `getDeclaredMethod on null` ×28 is extinct (0 instances in the clean 2026-06-30/07-01 re-runs + fresh 196-class sweep). Doc: `docs/internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md`. |
| `springrepos-…` (latent deep recursion) | `springrepos-deep-recursion/GroovyNestProbe.java` | deeply-nested Groovy closures; clean `dev` runs slow-not-crash — the native-stack overflow only with the unmerged cold-path JIT experiment. |
| `jit-regalloc-callee-saved-clobber-family` | `jit-regalloc-dup_x1/Dupx.java` | the bare `tab[index++]` `dup_x1` idiom — **does NOT** reproduce alone (matches HotSpot); kept as the negative control showing the family bug is method-shape-specific. |
| `wildfly-domain-hc0053-server-inventory-timeout` (blocking finding, not the doc's own root cause) | `wildfly-hc0053-aqs-stw-hang/AqsContentionProbe.java` | N (>=8) threads hammering one shared `ReentrantLock` + concurrent `System.gc()` pressure → hangs (`STW cross-thread JIT takeover ... taken=0`, whole-VM freeze) on `--release` dev tip; N<=7 clean. Zero WildFly/jboss-threads involved — isolates the STW/AQS-contention bug that blocked live `WFLYHC0053` verification, likely same root cause as `wip/gc-stw-quota-race-20260710`. HotSpot: clean at any N. |

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
