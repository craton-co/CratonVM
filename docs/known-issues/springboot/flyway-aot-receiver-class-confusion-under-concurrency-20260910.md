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

## Re-measured 2026-09-10, later the same day: it reproduces ALONE

**The concurrency in this page's title is no longer needed to see it, and the
title is now wrong.** Against `dev` at `0d18c01bd` (plus the inline-locals-floor
fix), one class in one process, no interleaving and no sibling arms:

| arm | runs | result |
|---|---|---|
| CratonVM, JIT on | 9 | **9 failed** (1 of 3 tests, every run) |
| CratonVM `--nojit` | 4 | 4 clean |
| stock HotSpot 25 | 3 | 3 clean |

So it is a JIT defect that now fails deterministically in a single process.
When this page was written the same class passed 6 of 6 alone and needed an
interleaved 8-way burst to fail 4 of 40; the vector got sharper on its own as
`dev` moved. **Do not start from the concurrency harness — one process
reproduces it.**

### The signature, which is now legible

```text
NoSuchMethodError: 'boolean org.springframework.context.annotation.ScopedProxyMode
                    .isAssignableFrom(java.lang.Class)'
  method="org/springframework/context/annotation/ScopedProxyMode.isAssignableFrom(Ljava/lang/Class;)Z"
  caller="org/springframework/util/ClassUtils.isAssignable(Ljava/lang/Class;Ljava/lang/Class;)Z @pc=17"
```

`ClassUtils.isAssignable(Class<?> lhsType, Class<?> rhsType)` at pc 17 is
`lhsType.isAssignableFrom(rhsType)`. The receiver there must be a
`java.lang.Class`; the VM found a `ScopedProxyMode` enum CONSTANT and resolved
the call against its class. That is the same shape as the two symptoms this
page was filed for (`AbstractMethodError: AnnotatedElement.getDeclaredAnnotations
() has no Code attribute` and `NoSuchMethodError: String.isPrimitive()`) and it
is the sharpest of the three, because both the wrong receiver and the slot it
belongs to are named.

### It is NOT the inline-locals floor

Filed with a note that it might share a cause with the Kafka row. It does not.
The floor fix (`internal/fixed-bugs/inline-locals-floor-moved-reservations-that-overlapped-nothing-FIXED-20260910.md`)
closes Kafka completely and leaves this one exactly where it was: interleaved,
same host, same session, pre-fix binary 2 of 3 failed and post-fix binary 3 of
3 failed. Try the Kafka reproducer FIRST is no longer the advice; that door is
closed.
