# Spring Framework full-suite GC-variant run — RETIRED: three defects, not one "classpath gap"

**Status: RETIRED 2026-08-10.** Supersedes
`docs/known-issues/spring/gc-variant-fullsuite-classpath-gap-and-fails-20260810.md`.
Every finding on that page is either fixed or superseded below, and the
full 2848-class × 3-GC-variant sweep has been re-run against a binary and
harness carrying all three fixes.

The original page's headline — "the dominant cause is an incomplete harness
classpath, not a CratonVM defect" — was **half right**. There was a harness
classpath bug, and it was dominant. But its *mechanism* was not what the page
described, and two further defects were hiding underneath it, one of them a
real CratonVM bug in exactly the cluster the page nominated as most likely to
be real.

| # | Defect | What it cost | Kind |
|---|---|---|---|
| 1 | `dumpTestCp` dumped jar paths it never built | 789 of 1156 failure-cause lines (68%) | harness |
| 2 | Vintage deprecation notice promoted to fatal | 40 of the 81 FAILs that survived #1 | harness |
| 3 | `sun.reflect.misc` frames skipped as reflection plumbing | 58 `javax.management.*` lines, 22 classes | **CratonVM** |

---

## 1. The classpath gap — right symptom, wrong mechanism

The original page concluded:

> This reads as `run-suite.sh`'s `discover` step indexing test classes from every
> built module (hence a 2848-class `all-classes.tsv`), while its classpath
> generator only ever assembled entries for a narrower subset of modules — the
> two steps have drifted apart.

and prescribed: *"diff `common.args`'s module directory list against
`spring-framework/settings.gradle`'s full module list to see exactly which
modules never made it in."*

**That prescription could not have worked, and the diagnosis is refuted.**
There is no `common.args` in `apps/spring-suite-runner` — that file belongs to
the hib/netty/quarkus runners. The Spring harness uses a *per-module*
`build/cratonvm-testcp.txt`, and:

* all 24 java-plugin modules were indexed, and
* all 24 had a `cratonvm-testcp.txt`.

No module was ever "missing from the list". Counting entries instead of
modules is what finds it:

```
$ for f in */build/cratonvm-testcp.txt; do ... [ -e "$e" ] || echo MISSING ...
spring-web        entries=204  missing=2
spring-test       entries=253  missing=5
framework-docs    entries=109  missing=8
...
```

The classpaths were complete as *lists*; the **files they named did not
exist**. Only 31 of the required jars were on disk.

### Mechanism

`sourceSets.test.runtimeClasspath` is a lazily-resolved `FileCollection`. For a
cross-project (`project(":spring-oxm")`) or test-fixtures dependency, Gradle
resolves it to that project's **published artifact** —
`build/libs/<name>-<ver>.jar` — not to its `build/classes/java/main` directory.
`dumpTestCp` called `.asPath` inside `doLast`, which asks only for the *names*
of those files. It never asked Gradle to **produce** them, because it never
declared the collection as a task dependency. So `./gradlew testClasses
dumpTestCp` wrote a perfectly well-formed classpath naming ~24 jars the build
had never been asked to create.

**A JVM silently skips a missing classpath element.** No warning, no non-zero
exit — the classes simply are not there. That is why it surfaced only as
`NoClassDefFoundError` deep inside Spring/JUnit, which reads exactly like a VM
defect.

### This also explains the package histogram the original page could not

The page grouped the ~600 non-AOT `NoClassDefFoundError` targets by top package
segment and read it as "`spring-web`, `spring-jms`, `spring-orm`,
`spring-context-support` and `spring-oxm` all read as entirely or mostly
missing". Two of those entries never fit that story. They fit this one exactly:

| package in the histogram | actually from | why |
|---|---|---|
| `org/springframework/web/` (177) | **spring-websocket** | `web/socket/**` is spring-websocket's own package; its jar was absent |
| `org/springframework/ui/` (42) | **spring-context-support** | `ui/freemarker` lives there — same module as cache (64), mail (18), scheduling (12) |
| `org/springframework/aot/` (129), `core/` (70) | **spring-core-test** | `aot/test/**`, `core/test/**` |
| `oxm/` (43), `jms/` (148), `orm/` (66) | those modules | own jars absent |
| `sun/reflect/misc/` (9) | — | **not this bug.** See §3 |

Every cluster maps one-to-one onto a module whose jar was missing. The one
cluster that does not — `sun/reflect/misc/` — is defect #3, which the original
page listed in this table and left unexplained.

### Fix

`dump-testcp.init.gradle` now declares `dependsOn
p.sourceSets.test.runtimeClasspath`, so asking for the task builds every
artifact it is about to name.

The file itself had to be committed first: it existed **only on the Azure box**,
untracked under the blanket `apps/` ignore, despite `run-suite.sh` referring to
it by name in a comment. `KRun.java` had the same problem.

### Guard, proven red before green

`run-suite.sh` gained `check-cp` and `dumpcp`, and `run`/`quad`/`hotspot` now
**refuse to start** when any classpath entry does not exist
(`ALLOW_INCOMPLETE_CP=1` overrides deliberately). A pass rate measured against
a silently-truncated classpath is not a pass rate, and must not be producible
by accident.

The guard was shown to fire before it was trusted: hiding
`spring-oxm-7.1.0-SNAPSHOT.jar` makes `check-cp` exit 1 and name all five
dependent modules (`spring-jms`, `spring-messaging`, `spring-oxm`,
`spring-test`, `spring-web`); restoring it returns exit 0.

An absent **source-set output directory** is deliberately *not* a failure —
Gradle puts one on the path whether or not it produced anything and does not
create it when empty. Four are expected here, and `spring-aspects` is the
instructive one: it has 27 test sources but no `build/classes/java/test`,
because `ajc` compiles them to `build/classes/aspectj/test`, which *is* present
and *is* on the path. A missing **jar** stays hard: once `jar`/`testFixturesJar`
runs, Gradle always writes the file even for an empty project, so its absence
always means the task never ran.

---

## 2. The JUnit Vintage cluster — a second harness bug, never classpath-related

The original page put `DiscoveryIssueException` (40) in the "suspect until the
classpath is fixed" bucket, alongside the bean-factory exceptions. It was not
downstream of the classpath at all — it came back at **exactly** its old count
of 40 after the classpath was repaired.

`run-suite.sh` copied `TestConventions.java`'s system properties, including:

```java
"junit.platform.discovery.issue.severity.critical", "INFO"
```

but not `spring-test/spring-test.gradle`'s companion:

```groovy
// Since spring-test relies on the Vintage test engine for JUnit 4 support,
// we disable reporting of the "deprecated" discovery issue, because that
// would otherwise fail the build.
systemProperty("junit.vintage.discovery.issue.reporting.enabled", "false")
```

Those two are a pair. `severity.critical=INFO` promotes every discovery issue of
severity INFO **or worse** to fatal, and the Vintage engine emits an INFO-level
"this engine is deprecated" notice on *every* discovery. Taking the first flag
without the second turns that notice into a `DiscoveryIssueException` and fails
the class before a single test runs:

```
FAILCAUSE ...BasicVintageTests :: JUnit Vintage :: DiscoveryIssueException:
  TestEngine with ID 'junit-vintage' encountered a critical issue during test discovery:
  (1) [INFO] The JUnit Vintage engine is deprecated and should only be used temporarily...
```

Spring's own build comment says exactly what would happen. Applied
unconditionally in the harness rather than per-module, because spring-test is
the only module with junit-vintage on its test classpath (verified against
every module's `cratonvm-testcp.txt`), so the flag is inert elsewhere.

---

## 3. `sun.reflect.misc.MethodUtil` — the real CratonVM defect

The original page's one correct instinct:

> The `javax.management.*` (58 combined) and `MockitoException` (25) clusters
> are more likely to be independent, worth a first look once the classpath is
> fixed.

**The `javax.management` half was right; the Mockito half was not.** After the
classpath fix the `MockitoException` cluster went to **zero** — it was classpath
noise. The `javax.management` cluster came back at its full 58, and it shares a
root cause with the 6 `NoClassDefFoundError`s that survived §1:

```
NoClassDefFoundError: sun/reflect/misc/MethodUtil
RuntimeErrorException: Error occurred in RequiredModelMBean while trying to invoke operation add
MBeanException: An exception occurred while trying to get an attribute value: ... invoke operation getName
```

`jimage list` confirms `sun/reflect/misc/MethodUtil.class` **is** in `java.base`
of the JDK 25 image. CratonVM was failing to load a class that exists.

### Mechanism

Isolated with a two-line probe (`probes/MU.java`) against a HotSpot control:

```
HotSpot:  OK    sun.reflect.misc.MethodUtil  loader=null  super=java.security.SecureClassLoader
CratonVM: FAIL  sun.reflect.misc.MethodUtil  -> InternalError: bouncer cannot be found
            caused by InaccessibleObjectException: Unable to make member accessible:
            module java.base does not "opens sun.reflect.misc" to unnamed module
```

`MethodUtil.<clinit>` calls `setAccessible(true)` on `Trampoline.invoke` from
`MethodUtil$1`. Both are in `java.base`, so same-module access is
unconditionally allowed on HotSpot.

`resolve_caller_class_id` (`native-builtins/src/lang_class.rs`) skips every
frame matching `REFLECTION_INTERNAL_CLASSES`, which includes `sun/reflect/`.
That prefix dates from the **pre-JDK-9** accessor classes
(`sun.reflect.NativeMethodAccessorImpl` and friends), which moved to
`jdk.internal.reflect` in JDK 9. In a real JDK 25 image the only things left
under `sun/reflect/` are `annotation/`, `generics/`, `ReflectionFactory` and
`misc/` — none of which sit between a caller and a reflection native.

So the `MethodUtil$1` frame was skipped, the walk continued out to the
application class, the caller was judged to be the **unnamed module**, and the
`opens` check refused. `MethodUtil.<clinit>` wraps that as `InternalError:
bouncer cannot be found`, so every later use surfaced as a slash-name
`NoClassDefFoundError` — pointing nowhere near reflection.

The JDK's `javax.management.modelmbean.RequiredModelMBean` routes **every**
managed operation through `MethodUtil`, which is how one skipped frame became
58 JMX-wrapped failures across 22 `org.springframework.jmx.*` classes.

### Fix

A new `REFLECTION_INTERNAL_EXCEPTIONS` carve-out, containing only
`sun/reflect/misc/`. Narrow on purpose: widening it to all of `sun/reflect/`
would be **fail-open** if an accessor-like class ever lands there again, and
that skip list is what keeps `Method.invoke` attributed to real user code.

### Verified three ways

* the fixed binary matches HotSpot on all four probe lines — **including** the
  `Trampoline` `Error: Trampoline must not be defined by the bootstrap
  classloader`, which both produce, so the probe is not degenerate;
* the pre-fix binary still shows the red on the same probe;
* **33/33** `org.springframework.jmx.*` classes pass, zero `FAILCAUSE` lines
  (22 of them were failing before).

`native-builtins` cannot host a unit test of its own (~620 pre-existing compile
errors in its test targets from the fallibility migration), so the verification
is behavioural, with the HotSpot control as the oracle.

---

## Results — full 2848-class index, 3 GC variants

Re-run 2026-08-10 on Azure Linux (`20.80.105.49`, 8 cores), real JDK 25, JIT on,
one `--features zgc` binary selecting the collector by runtime flag, 8 shards
per variant, variants sequential. Index unchanged at 2848 classes.
`out/gcfinal-20260810-143809/`.

| variant | OK | FAIL | LOADERR | TIMEOUT | EMPTY | total | | was OK |
|---|--:|--:|--:|--:|--:|--:|---|--:|
| default | **2819** | 18 | 0 | 11 | 0 | 2848 | | 2491 |
| G1 | **2824** | 20 | 0 | 4 | 0 | 2848 | | 2499 |
| ZGC | **2823** | 20 | 0 | 5 | 0 | 2848 | | 2494 |

**87.5% → 99.0 / 99.2 / 99.1%.** `LOADERR` 13 → **0**. `EMPTY` 4 → **0** (those
four were discovery casualties of §1/§2, not empty classes). Zero
`cratonvm::gc::guard` hits of any kind, in any variant — the original page's
one uncontested finding still holds.

Failure causes, all three variants pooled (was: 789 `NoClassDefFoundError`,
61 `UnsatisfiedDependencyException`, 40 `DiscoveryIssueException`, 58
`javax.management.*`, 25 `MockitoException`, …):

```
64 java.lang.AssertionError                        1 org.springframework.aop...RetryableException
21 java.util.NoSuchElementException                1 org.mockito.exceptions.base.MockitoException
15 java.lang.RuntimeException                      1 java.lang.IllegalStateException
12 org.springframework.beans.factory.BeanCreationException
12 org.opentest4j.AssertionFailedError
 3 org.springframework.test.context.aot.TestContextAotException
 2 org.springframework.web.client.ResourceAccessException
 1 org.springframework.beans.factory.BeanDefinitionStoreException
```

`NoClassDefFoundError`: **gone**. `DiscoveryIssueException`: **gone**.
`javax.management.*`: **gone**. `UnsatisfiedDependencyException` /
`NoSuchBeanDefinitionException` / `BeanDefinitionParsingException`: **gone** —
the original page's suspicion that the bean-wiring exceptions were secondary
symptoms of the classpath gap was correct. `MockitoException` 25 → **1 across
all three variants**; its suspicion about *that* cluster being independent was
not.

### The residual, against a HotSpot control

`triage-vs-hotspot.sh` re-ran the default variant's 29 non-passing classes under
HotSpot 25 with the same harness, classpath and JVM args:

| verdict | count |
|---|--:|
| `CRATONVM-DEFECT` (fails here, passes on HotSpot) | 28 |
| `BOTH-FAIL` (not a CratonVM signal) | 1 |

The single `BOTH-FAIL` is `aot.nativex.FileNativeConfigurationWriterTests`.

The HotSpot control ran serially on an idle box while the sweep ran 8-way
sharded under load ~13, so the TIMEOUTs were **not** a controlled comparison.
Re-running all 11 of them serially on an idle box (load 0.20) separates them:

| | count | classes |
|---|--:|---|
| genuine — still TIMEOUT unloaded | 9 | `AotIntegrationTests`, `BeanRegistrationsAotContributionTests`, `RestClientIntegrationTests`, `WebClientIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`, `SseIntegrationTests`, `WebSocketIntegrationTests`, `ServletAnnotationControllerHandlerMethodTests`, `ParallelExecutionSpringExtensionTests` |
| host-load artifact — OK unloaded | 2 | `CrossOriginAnnotationIntegrationTests` (70 s), `MultipartWebClientIntegrationTests` (45 s) |

So the honest residual is **26 genuine CratonVM divergences out of 2848
(0.9%)**, and the default column reads 2821 / 18 / 9 once the two load
artifacts are credited.

They cluster as follows (8 + 2 + 9 + 7 = 26), and none of them belongs to this
page:

* **XML / XSLT (8)** — `XsltViewTests` (`NoSuchElementException: No value
  present`), `XmlExpectationsHelperTests`, `ContentRequestMatchersTests`
  ("Failed to parse expected or actual XML request content"), four
  `XmlContent*`/`XmlContentAssertion*` classes ("XML parsing error").
* **Reactive / Netty integration hangs (9)** — the TIMEOUT set above.
* **AOT generation (2)** — `ApplicationContextAotGeneratorTests` and
  `TestContextAotGeneratorIntegrationTests` (the latter is the
  `TestContextAotException`). Two more AOT classes hang and are counted in the
  TIMEOUT row above, not here.
* **Singletons (7)** — `AutowiredAnnotationBeanPostProcessorTests`,
  `Spr15042Tests`, `BshScriptFactoryTests`, `RestOperationsExtensionsTests`,
  `RestTemplateIntegrationTests`, `ControllerAdviceTests`,
  `StompWebSocketIntegrationTests`.

### The original page's spot-check, resolved

It flagged
`AutowiredAnnotationBeanPostProcessorTests.genericsBasedFieldInjectionWithSubstitutedVariables`
("Expected size: 1 but was: 4") and left it open — *"could be a real
classpath-scanning/generics-resolution difference, or could still be an artifact
of the same module-visibility gap"*.

**Resolved: it is not a classpath artifact.** It still fails against a
verified-complete classpath and passes on HotSpot — `CRATONVM-DEFECT`. It is a
real generics-resolution divergence and needs its own page.

---

## What the original page got right, and what it should have done differently

Right:
* the dominant cause really was a harness bug, and saying so loudly was correct;
* "do not treat any of this run's FAIL counts as a CratonVM pass-rate baseline"
  was exactly the right call;
* nominating `javax.management` as most likely to be independent was correct.

Should have been done differently:
* **No HotSpot control was ever run.** One control pass over the non-passing set
  would have shown HotSpot failing on the same 789 classes and named the harness
  on day one, instead of a package-by-package histogram that still could not
  explain `org/springframework/web/` or `org/springframework/ui/`.
  `apps/spring-suite-runner/triage-vs-hotspot.sh` now does exactly this and is
  committed alongside the sweep driver.
* **The prescribed next step named a file that does not exist in this harness**
  (`common.args`). Reading the fixture first would have redirected the whole
  investigation in one `ls`.
* **"Everything downstream is unreliable" over-generalised.** It swept the
  Vintage cluster (§2) and the JMX cluster (§3) into the same bucket as the bean
  wiring noise. Two of the three defects here were invisible for that reason —
  and the bean-factory exceptions it *did* suspect really were classpath noise
  and really did evaporate.

## Explicitly out of scope

The original page's "Related" section suggests checking
`aotintegration-hangs-after-the-unmodifiable-get-fix.md` for overlap with the
`org/springframework/aot/` cluster. That cluster is fully accounted for by the
missing `spring-core-test` jar (§1) — `org.springframework.aot.test.**` lives in
that module. The overlap check itself was **not** performed and that page was
**not** modified; it is out of scope for this retirement.

## Landed

Branch `fix/spring-suite-testcp-jar-artifacts-20260810`:

| commit | what |
|---|---|
| `d2a4ba658` | `dumpTestCp` builds the jars it names; `check-cp`/`dumpcp`; JDK25 detection; force-add `dump-testcp.init.gradle` + `KRun.java` |
| `582445394` | absent source-set output dir is not a classpath defect |
| `de9ec1a5e` | resolve JDK tools by name, not a hardcoded `.exe` |
| `68965305a` | `gc-variant-sweep.sh` |
| `0b40d7966` | `triage-vs-hotspot.sh` |
| `f18a81b2e` | carry `spring-test.gradle`'s vintage flag |
| `35ec7fc16` | **`sun.reflect.misc` is a caller, not reflection plumbing** |
| `8d7435bb4` | mark the two new drivers executable |

Two portability residuals the original page never mentioned were fixed along the
way: `JDK25`/`JDK25_WIN` were a hardcoded Windows path, and `javac.exe`/`java.exe`
were hardcoded — the entire `hotspot` baseline mode could never have run on the
Azure Linux box. That was masked because `compile_krun` returns early when
`KRun.class` exists, and a class file carried over from a Windows run satisfied
the check.

## Related

* `../../../known-issues/h2/gc-corruption-guard-fixed-by-dev-merge-20260810.md`
  — the `dev`-merge story that prompted this run. Its "No regression check"
  caveat is now closed **for spring-framework**: this sweep re-ran all 2848
  classes, not just the previously-non-passing set.
* `../../../known-issues/h2/gc-variant-fullsuite-crashes-hangs-fails-20260810.md`
  — H2's half of the same sweep, where the non-passing set *is* dominated by
  real CratonVM defects.
