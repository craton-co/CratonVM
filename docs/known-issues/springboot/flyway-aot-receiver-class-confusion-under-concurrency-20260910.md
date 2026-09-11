# A JIT'd Spring type check reads a `String` where a `Class` belongs

**Status: OPEN, 2026-09-10.** `module/spring-boot-flyway …
ResourceProviderCustomizerBeanRegistrationAotProcessorTests` is a CratonVM-only
Spring Boot failure whose HotSpot 25 baseline is `PASS 0/3`. It is **not** a
Flyway or AOT-substitution problem — the AOT substitution itself is fine
(`flyway-resourceprovidercustomizer-aot-substitution-not-applied-FIXED.md`
closed that in an earlier pass and has not regressed). It is a wrong RECEIVER
in JIT-compiled code, and it only shows up when several JVMs run at once.

## Two symptoms, one shape

The suite run (`full_zgc8_*`, 2026-09-10, 8 shards) recorded:

```text
BeanInitializationException: Failed to process @EventListener annotation on bean
  with name 'applicationStartup':
  method java/lang/reflect/AnnotatedElement.getDeclaredAnnotations()[Ljava/lang/annotation/Annotation;
  has no Code attribute
  at org.springframework.core.annotation.AnnotationsScanner.getDeclaredAnnotations(AnnotationsScanner.java:439)
  at …AnnotatedElementUtils.isAnnotated(AnnotatedElementUtils.java:216)
  at …EventListenerMethodProcessor.isSpringContainerClass(EventListenerMethodProcessor.java:212)
```

The reproduction here records:

```text
java.lang.NoSuchMethodError: 'boolean java.lang.String.isPrimitive()'
  at org.springframework.util.ClassUtils.isAssignable(ClassUtils.java:629)
  at org.springframework.core.ResolvableType$1.isAssignableFrom(ResolvableType.java:1128)
  at org.springframework.core.ResolvableType.isInstance(ResolvableType.java:249)
  at …AbstractBeanFactory.isTypeMatch(AbstractBeanFactory.java:591)
  at …DefaultListableBeanFactory.getBeanNamesForType(DefaultListableBeanFactory.java:604)
```

Read them together. `ClassUtils.isAssignable(Class lhsType, Class rhsType)`
calls `lhsType.isPrimitive()`; the receiver's runtime class came back
`java.lang.String`. `AnnotationsScanner.getDeclaredAnnotations(AnnotatedElement
source)` calls `source.getDeclaredAnnotations()` on what is a `Class`; a
receiver that does NOT implement `AnnotatedElement` leaves interface dispatch
with nothing but the interface's own abstract method, which is exactly what
"has no Code attribute" reports. **Both are one defect: the receiver of a
`Class`-typed local is some other object.** They differ only in which
bytecode reached the substituted receiver first.

## Measurement

Azure Linux (`20.80.105.49`), `dev`@`39a90d2f4` + the unrelated generics fix,
one process per run through `sb-runner`, `--XX:UseGc Z`.

**Alone on a quiet host the class passes.** 6 of 6 runs clean, load 3-6. That
is why this row reads as flaky rather than broken, and why a single re-run
"clears" it.

**Under concurrency it reproduces, and it is JIT-gated.** Four JIT and four
`--nojit` processes launched in the SAME burst, ten rounds — the arms are
paired inside each burst rather than run as separate tables, so neither arm
owns a quieter host than the other:

| arm | failed | runs | rate |
|---|---:|---:|---:|
| CratonVM, JIT on | **4** | 40 | 10 % |
| CratonVM, `--nojit` | **0** | 40 | 0 % |

All four failures carry the identical `String.isPrimitive()` signature. Host
load ran 30-39 across all ten rounds.

An earlier, sequential 8x3 pass (JIT only) read 2 failures / 24 runs, and the
same script re-run later read 0 / 24 — consistent with ~10 % and a reminder
that 24 runs is not enough to price this vector. Do not conclude anything about
this row from fewer than ~40 paired runs.

## What is NOT the cause

* Not the Flyway AOT substitution: the failing method is
  `shouldReplaceResourceProviderCustomizer`, but the failure lands in Spring's
  bean-factory type matching / `@EventListener` scan during `refresh()`, before
  any assertion about the substitution runs.
* Not a collector: reproduced under `--XX:UseGc Z`; the suite shard that
  recorded it was also ZGC. Not re-priced against Generational/G1 (this vector
  needs ~40 paired runs per arm, which is expensive; do it before claiming a
  collector is exempt).
* Not host privilege or a Windows gap: this host is Linux and the class needs
  nothing special.

## Relationship to the Kafka row

`internal/fixed-suite-bugs/springboot/kafka-scala-statics-anyhash-jit-miscompile-FIXED-20260910.md`
is a JIT-only, nondeterministic wrong-VALUE defect in a compiled body;
this is a JIT-only, nondeterministic wrong-RECEIVER defect. They may be the
same underlying frame/slot fault seen through two different consumers, and the
Kafka one has a twenty-line reproducer that needs no concurrency at all.
**Try the Kafka reproducer first**; if it resolves, re-price this row before
investigating it separately.

## How to reproduce

```bash
# 4 JIT + 4 --nojit in the same burst, 10 rounds
bash /data/flyab.sh 10        # see the script in this page's session notes
```

or, minimally, launch eight concurrent

```bash
/data/sbone.sh module/spring-boot-flyway \
  org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizerBeanRegistrationAotProcessorTests \
  craton --XX:UseGc Z
```

and expect roughly one failure per ten JIT processes.


## Re-measured 2026-09-10, later the same day

Still OPEN. What changed is the quality of the measurement, not the verdict.
Three things below: a correction, a cleared suspect, and a localisation that
came back empty.

### CORRECTION: "it reproduces alone" was a load artefact

An earlier revision of this section reported the class failing **9 runs of 9
alone**, concluded that the concurrency in this page's title was no longer
needed, and told the next reader not to bother with the concurrent harness.
**That was wrong, and wrong in the way this page already warned about.**

Those nine runs were sequential, on a host whose 15-minute load average was in
the 80s. Twenty minutes later, at load 34, the same binary ran the same class
and passed **3 of 3** — and a seven-arm sweep taken sequentially in that window
read `0 tests failed` for six arms and `1 tests failed` for the seventh, which
would have "localised" the defect to whichever arm the load happened to land
on.

The **Method** section above says to put every arm in one burst precisely so
that load cannot be confounded with the arm. The correction is not new
knowledge; it is this page's own instrument being ignored by the person who
wrote it. Every number below comes from a single burst with the control in it.

### The floor fix is NOT this bug — measured, and the first read was noise

`internal/fixed-bugs/inline-locals-floor-moved-reservations-that-overlapped-nothing-FIXED-20260910.md`
closes the Kafka row completely and was filed with a note that this row might
share its cause. It does not.

| burst | arm | failed / runs |
|---|---|---|
| 3-arm, 16 each | pre-floor-fix binary, JIT | 11 / 16 |
| | post-floor-fix binary, JIT | 15 / 16 |
| | post-floor-fix binary, `--nojit` | **0 / 16** |
| 4-arm, 24 each | pre-floor-fix binary, JIT | **21 / 24** |
| | post-floor-fix binary, JIT | **21 / 24** |

The first burst read 11 against 15 and looked like the floor fix had made this
vector worse. **It had not.** At 24 runs each the two arms are the same number.
A frame-layout change can move a latent slot defect's rate in either direction,
which is exactly why a 16-run gap of that size is not a result; it is recorded
here so that nobody re-derives the scare from the smaller burst.

Direct evidence to the same effect: with `CRATONVM_DBG=jit-locals-floor` and
`CRATONVM_DBG=jit-slot-overlap` on a failing run, there are **0 ENCLOSING
overlap rows** and **no floor row names `ClassUtils`, `Assert`, or any frame in
the stack below**. The floor neither engages here nor was engaging here before.

`--nojit` at 0 of 16 remains the cleanest statement available: this is codegen,
not a library or classpath difference.

### The signature and the whole caller chain

All failures in these bursts carry one message:

```text
NoSuchMethodError: 'boolean org.springframework.context.annotation.ScopedProxyMode
                    .isAssignableFrom(java.lang.Class)'
  caller="org/springframework/util/ClassUtils.isAssignable(Ljava/lang/Class;Ljava/lang/Class;)Z @pc=17"

  org.springframework.util.ClassUtils.isAssignable(ClassUtils.java:622)
  org.springframework.core.ResolvableType$1.isAssignableFrom(ResolvableType.java:1128)
  org.springframework.core.ResolvableType.isInstance(ResolvableType.java:249)
  org.springframework.beans.factory.support.AbstractBeanFactory.isTypeMatch(:591)
  org.springframework.beans.factory.support.DefaultListableBeanFactory.doGetBeanNamesForType(:639)
  org.springframework.beans.factory.support.DefaultListableBeanFactory.getBeanNamesForType(:604)
  org.springframework.context.support.DefaultLifecycleProcessor.getLifecycleBeans(:531)
```

The bytecode pins the slot exactly. `ClassUtils.isAssignable(Class lhsType,
Class rhsType)` is

```text
 0: aload_0 / ldc "Left-hand side type must not be null"  / invokestatic Assert.notNull
 6: aload_1 / ldc "Right-hand side type must not be null" / invokestatic Assert.notNull
12: aload_0
13: aload_1
14: invokevirtual java/lang/Class.isAssignableFrom:(Ljava/lang/Class;)Z
17: ifeq 22
...
22: aload_0
23: invokevirtual java/lang/Class.isPrimitive:()Z
```

So **local 0 (`lhsType`) holds an object that is not a `java.lang.Class`** by
the time pc 12 reads it — and `Assert.notNull` at pc 0-5 already read the same
local and did not throw. This unifies all three symptoms this page carries:

| symptom | which read of local 0 | what it held |
|---|---|---|
| `ScopedProxyMode.isAssignableFrom(Class)` | pc 12 | a `ScopedProxyMode` enum constant |
| `String.isPrimitive()` | pc 22 | a `java.lang.String` |
| `AnnotatedElement.getDeclaredAnnotations() has no Code attribute` | elsewhere | an unrelated interface receiver |

A `String` instance rules out the tempting reading that a `Class` MIRROR is
dispatching against the class it represents; local 0 really does hold an
unrelated live object. The values are long-lived ones (an enum singleton
reachable from a static array, an interned-looking string), which points at a
stale or cross-frame reference rather than an arithmetic result.

### Localisation came back EMPTY, and that is the finding

Ten rounds, four arms per round, control in every round (`/data/flyab5.sh`):

| arm | failed / runs |
|---|---|
| control | 19 / 20 |
| `CRATONVM_JIT_DENY=org/springframework/core/ResolvableType` | 13 / 20 |
| `…/beans/factory/support/DefaultListableBeanFactory` | 17 / 20 |
| `…/beans/factory/support/AbstractBeanFactory` | 16 / 20 |

and from the earlier 4-arm burst, `CRATONVM_JIT_DENY=org/springframework/util/ClassUtils`
was **22 of 24** — force-interpreting the very method that reads the bad local
does not clean it.

**No class-level deny clears this vector.** `ResolvableType` at 13/20 against a
19/20 control is the only arm that moves at all, and it is not significant at
this size (one-sided Fisher p ~ 0.06) — nor would it be a localisation if it
were, because denying a whole class changes far more than one method's
compilation.

Read together with `ClassUtils` being innocent, that says the wrong value is
**already in the argument** by the time `isAssignable` is entered, and is not
manufactured by any single one of the four frames above it. That is consistent
with the load dependence: the defect needs something that only happens under
scheduling pressure — a safepoint, a deopt, an OSR entry, or a GC — rather than
a fixed miscompile in one body.

### What the next reader should do

1. **Use the concurrent harness.** `/data/flyab3.sh`, `/data/flyab4.sh`,
   `/data/flyab5.sh` on the Azure host are three shapes of it; the **Method**
   section above is the rule. One process at low load proves nothing in either
   direction, and a sequential sweep invents localisations.
2. **Stop deny-sweeping by class.** Four classes across the chain, including the
   reader of the bad slot, and none of them clears it.
3. Go after the mechanism instead: what writes local 0 of a compiled/spliced
   `ClassUtils.isAssignable` between pc 5 and pc 12, or what makes its frame's
   slot 0 be read from the wrong place. A deopt or OSR transition that restores
   locals from a mismatched map, and a GC that relocates while a slot is
   described wrongly, are both consistent with every number above -- including
   `--nojit` being clean and no single method being at fault.
