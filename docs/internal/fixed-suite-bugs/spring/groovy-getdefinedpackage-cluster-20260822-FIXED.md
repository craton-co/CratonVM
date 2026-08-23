# The Spring Groovy cluster — `getDefinedPackage` could not see what `definePackage` wrote

## Status
**FIXED 2026-08-22.** All 11 Groovy classes equal HotSpot: 32/103 → **103/103**
test methods. `probes/DefinedPackageProbe.java` reproduces the whole thing with
no Groovy on the classpath and is now byte-identical to HotSpot on all 23 rows.

## The failure

```
org.springframework.beans.factory.parsing.BeanDefinitionParsingException:
  Configuration problem: Error evaluating Groovy script: org.springframework.context.groovy
Offending resource: class path resource [org/springframework/context/groovy/applicationContext.groovy]

Caused by: java.lang.IllegalArgumentException: org.springframework.context.groovy
	at java.lang.ClassLoader.definePackage(ClassLoader.java:2068)
	at groovy.lang.GroovyClassLoader.definePackageInternal(GroovyClassLoader.java:428)
```

An exception whose entire message is a package name is the tell: that is
`ClassLoader.definePackage`'s `IllegalArgumentException(name)`, which it throws
when the package is **already defined**.

## The mechanism

Two halves of one piece of state, in two different worlds:

* `ClassLoader.definePackage` runs **real JDK bytecode** against the loader's
  own `packages` map. Its contract is
  `packages.putIfAbsent(name, pkg) != null -> throw IllegalArgumentException`.
* `ClassLoader.getDefinedPackage` is a **CratonVM native**. It was introduced to
  dodge a null `packages` field in ByteBuddy's
  `JavaDispatcher$DynamicClassLoader`, and its own comment says what it does
  instead: *"derives a conservative answer from classpath-visible class files."*

A loader that defines classes from **source or bytes** — `GroovyClassLoader`,
every script engine, every ByteBuddy/CGLIB loader — has no class files. So the
query half answered null forever while the definition half had already recorded
the package. Groovy reads exactly that pair:

```java
Package pkg = getDefinedPackage(pkgName);
if (pkg == null) definePackage(pkgName, null, null, null, null, null, null, null);
```

First class in a package: query says null, define succeeds, map now holds it.
**Second class in the same package: query still says null, define throws.** Any
Groovy script defining two classes in one package dies — which is why the
damage scales with script size (`GroovyBeanDefinitionReaderTests` 3/36) rather
than being all-or-nothing.

**A regression**, bisected to the 2026-08-11..08-17 window by running the
suspect class against three older binaries: the change that replaced an
unconditional `null` with the classpath probe fixed Spring Boot's
`BeanDefinitionLoader.findPackage` and silently broke every loader that does
not load from the classpath. The positive half was pinned; the negative half
was not.

## The fix

`getDefinedPackage` now answers in three steps, and only the first is new:

1. **the receiver's own `packages` map** — the same state `definePackage`
   writes. When it holds a full `java.lang.Package`, that exact object is
   handed back, so `getDefinedPackage(p) == definePackage(p, ...)` as the JDK
   guarantees. When it holds the JDK's lazy `NamedPackage` (what defining a
   *class* stores), the package is still DEFINED and we supply the `Package`
   ourselves, memoised for identity stability;
2. the existing memo of packages this VM synthesised;
3. the existing classpath probe.

Every failure mode of step 1 falls through to step 3 rather than inventing an
answer: no loader object, an absent or null `packages` field, a lookup that
throws. **The ByteBuddy case this override exists for is untouched** — that
loader's field is null, which is exactly the fall-through.

`getDefinedPackages` unions the real map too, routing each name back through
`getDefinedPackage` so the plural method stops contradicting the singular one
and both hand out one object per package.

The default package is a package. `package_class_glob("")` is `*.class`; the
uniform derivation would give `/*.class`, an absolute path matching nothing,
which reads as "no classes in the default package" on every classpath. The
empty name is answered from class files only for a **built-in** loader — a
custom loader must not inherit the global classpath's view of it (probe row
N02 pins that, and it is the same loader-identity error dev's own
`aa09d8bd8` fixed one level up).

## Two fixes, opposite directions, same function

`aa09d8bd8` landed on `dev` while this was in flight and touches the same
resolution order. The two are **orthogonal and both required**:

| | direction | symptom |
| --- | --- | --- |
| `aa09d8bd8` (dev) | narrows OVER-claiming | `appLoader.getDefinedPackage("java.lang")` fabricated a `Package` for a package the BOOT loader defines |
| this change | repairs UNDER-claiming | a loader could not see packages it had defined itself |

Measured rather than assumed — a pristine `origin/dev` binary was built and run
against the probe and the cluster:

| | probe rows differing | Groovy cluster |
| --- | --- | --- |
| pristine `origin/dev` | 8 | 32/103 |
| merged | **0** | **103/103** |

The merge resolution keeps dev's segment-aware probe body verbatim and this
branch's `loader_is_builtin` helper — whose doc comment is *corrected* by dev's
finding, since its original wording ("built-in loaders ARE the global
classpath") is precisely the belief their bug disproved.

## Measurement

`probes/DefinedPackageProbe.java`, 23 rows, no Groovy required. Before: 8
diverge. After: 0.

```
G02 second class in SAME package   HotSpot: already-defined   was: THREW IAE: probe.pkg.beta
P04 getDefinedPackage after define is non-null   HotSpot: true   was: false
P07 same object definePackage returned           HotSpot: true   was: false
L01 definer sees it                              HotSpot: true   was: false
S01 getDefinedPackages contains the defined one  HotSpot: true   was: false
C02 app loader sees its own default package      HotSpot: non-null   was: null
```

Whole classes, HotSpot / pristine dev / merged:

| class | HS | dev | merged |
| --- | --- | --- | --- |
| `GroovyBeanDefinitionReaderTests` | 36/36 | 3/36 | **36/36** |
| `GroovyScriptFactoryTests` | 38/38 | 28/38 | **38/38** |
| `GroovySpringContextTests` | 6/6 | 0/6 | **6/6** |
| `AbsolutePathGroovySpringContextTests` | 6/6 | 0/6 | **6/6** |
| `RelativePathGroovySpringContextTests` | 6/6 | 0/6 | **6/6** |
| `GroovyApplicationContextTests` | 4/4 | 1/4 | **4/4** |
| `GroovyApplicationContextDynamicBeanPropertyTests` | 2/2 | 0/2 | **2/2** |
| `BasicGroovyWacTests` | 2/2 | 0/2 | **2/2** |
| `GroovyControlGroupTests` | 1/1 | 0/1 | **1/1** |
| `DefaultScriptDetectionGroovySpringContextTests` | 1/1 | 0/1 | **1/1** |
| `MixedXmlAndGroovySpringContextTests` | 1/1 | 0/1 | **1/1** |

## How the cluster was found, and what the sweep had said

A 1222-class Spring slice on the dev tip reported **16** non-OK classes, not the
3 a previous sweep's summary carried. Adjudicated individually:

| | verdict |
| --- | --- |
| the 11 Groovy classes | **this fix** |
| `AspectJTypeFilterTests` | flake — 8/8 on both binaries on re-run |
| `ObjectFactoryCreatingFactoryBeanTests` | flake — 9/9 on all three arms |
| `InstanceSupplierCodeGeneratorTests` | flake — 24/26 on both binaries |
| `BeanRegistrationsAotContributionTests` | throughput wall, see below |
| `WebClientIntegrationTests` | **still open**: HotSpot 169/170, CratonVM 167/170 — two genuine method failures, unchanged by this fix |

## Three defects in the measurement apparatus, found on the way

Recorded because each one silently corrupts triage:

* **`hs.sh` caps HotSpot at 300s** (`HS_TO` default) and the sweep drivers record
  a killed oracle as `NORESULT` — indistinguishable from a test that genuinely
  produced nothing. The first whole-class run of
  `BeanRegistrationsAotContributionTests` therefore reported "HotSpot produces no
  result either", which would have retired a class as not-a-CratonVM-bug on the
  strength of the runner's own stopwatch.
* **No HotSpot per-method runner existed.** `hsm.sh` mirrors `hs.sh` exactly
  except for the entry point, so a per-method oracle is comparable row-for-row
  with `onem.sh`. That is what turned "the class hangs" into a table.
  `apps/` is gitignored, so it CANNOT be committed and does not survive a
  `/data` sweep — the recipe, from the runner directory:

  ```bash
  sed -e 's/^CLS="\$1"; shift$/CLS="$1"; MTH="$2"; shift 2/'       -e 's#-cp "$CP" KRun "$CLS"#-cp "$CP" KRunM "$CLS" "$MTH"#'       hs.sh > hsm.sh && chmod +x hsm.sh
  ```

  `KRun`/`KRunM` also need compiling into the runner directory before either
  script works (`javac -cp "$(tr -d '' < <module>/build/cratonvm-testcp.txt)"
  -d . KRun.java KRunM.java`), and `meta/all-classes.tsv` must exist — a fresh
  worktree has neither, and the failure reads as "class not in index".
* **`fine` phase accounting attributes nothing**: `residual_ppm=994308` — 99.4%
  unattributed, every bucket zero except startup. `coarse` works.

## `BeanRegistrationsAotContributionTests` is not a defect

Every one of its 15 methods PASSES; the class exceeds the sweep's 420s cap
because three AOT code-generation methods are slow. It compiles generated
sources with javac in-process, and that is a general JIT code-quality gap:

| | wall |
| --- | --- |
| HotSpot | 9s |
| CratonVM, JIT on | 73s |
| CratonVM, `--nojit` | 183s |

Phase accounting puts effectively all of it in `java_execution` — nothing in GC,
class loading, compilation or native calls — and the JIT compiled 5,052 methods,
3,476 of them javac's, so it is not a coverage gap either. Two hypotheses were
tested and refuted: weak-reference cache thrashing (`probes/WeakCacheProbe.java`,
16 rows identical on both VMs across all three collectors) and JIT
non-engagement. Closing this means competing with C2 on a compiler workload —
a performance programme, not a bug fix.

## Repro

```bash
javac -d /tmp/cls probes/DefinedPackageProbe.java
java -cp /tmp/cls DefinedPackageProbe > /tmp/hs.txt
<cratonvm-bin> --java-home $JDK25 -cp /tmp/cls DefinedPackageProbe 2>/dev/null | diff /tmp/hs.txt -

cd apps/spring-suite-runner
JDK25=/data/toolchain/jdk-25 SPRING=/data/cratonvm/apps/spring-framework \
CRATONVM_BIN=<cratonvm-bin> ./one.sh \
  org.springframework.context.groovy.GroovyBeanDefinitionReaderTests
```

Diff on **stdout only** — CratonVM's tracing goes to stderr.
