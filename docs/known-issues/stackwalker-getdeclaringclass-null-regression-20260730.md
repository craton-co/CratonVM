# `StackWalker$StackFrame.getDeclaringClass()` returns null during a running `<clinit>` — again

**Status: OPEN (found 2026-07-30, dev `9ac1feffe`).** Not a JIT bug —
reproduces identically under `--nojit`. Same symptom, same call path, and the
same trigger condition as the bug closed at `61dbe35e6`, but the ClassId
threading that fix introduced is still present in the tree, so this is a
surviving or re-opened hole in that fix rather than a straight revert.

## Symptom

```
java.lang.NullPointerException: Cannot invoke "Object.equals(Object)" because
the return value of "java.lang.StackWalker$StackFrame.getDeclaringClass()" is null
	at org.apache.logging.log4j.util.StackLocator.lambda$getCallerClass$7(StackLocator.java:66)
	at java.util.stream.WhileOps$UnorderedWhileSpliterator$OfRef$Dropping.tryAdvance(WhileOps.java:793)
	at java.util.stream.Stream.dropWhile(Stream.java:830)
	at org.apache.logging.log4j.util.StackLocator.lambda$getCallerClass$9(StackLocator.java:67)
	at org.apache.logging.log4j.util.StackLocator.getCallerClass(StackLocator.java:66)
	at org.apache.logging.log4j.util.StackLocatorUtil.getCallerClass(StackLocatorUtil.java:113)
	at org.apache.commons.logging.impl.Log4jApiLogFactory$LogAdapter.getContext(Log4jApiLogFactory.java:161)
	at org.apache.logging.log4j.spi.AbstractLoggerAdapter.getLogger(AbstractLoggerAdapter.java:46)
	at org.apache.commons.logging.impl.Log4jApiLogFactory.getInstance(Log4jApiLogFactory.java:210)
	at org.apache.commons.logging.LogFactory.getLog(LogFactory.java:920)
	at org.springframework.core.SimpleAliasRegistry.<init>(SimpleAliasRegistry.java:47)
	at org.springframework.beans.factory.support.DefaultSingletonBeanRegistry.<init>(...)
	... DefaultListableBeanFactory.<init>
```

`StackLocator.getCallerClass` walks the stack with
`StackWalker.walk(s -> s.dropWhile(f -> f.getDeclaringClass().equals(...)))`;
the frames it walks back into include one whose class is still executing its
own `<clinit>` on this thread, and `getDeclaringClass()` answers Java `null`
for that frame.

## Affected classes (Spring framework suite, real JDK 25)

| Class | Result |
|---|---|
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | FAIL 0/14 — every test method, this NPE |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 |

`AutowiredAnnotationBeanRegistrationAotContributionTests` was **OK 14/14** as
recently as the "third session" closure recorded in
`docs/internal/fixed-suite-bugs/spring/CRATONVM-SPRING-GENUINE-BUGLIST.md`,
so this is a regression, not a never-worked case.

## What is already known

`CRATONVM-SPRING-GENUINE-BUGLIST.md` (the `ExceptionInInitializerError` on
`SpringFactoriesLoader`/`EntityManagerFactoryUtils` entry) documents the
identical failure and its fix, landed on dev at `61dbe35e6`:

> a stack frame's declaring class was captured as a STRING NAME at
> stack-capture time, and every `getDeclaringClass()` implementation
> re-resolved that name to a `ClassId` LATER, on demand, via a global
> by-name lookup — unreliable for a class whose own `<clinit>` is still
> executing on the same thread doing the walk

Fixed then by threading the frame's own `ClassId` through
`StackTraceEntry` (`native-api/src/registry.rs`) and eagerly resolving the
`Class` mirror in `populate_stack_frame` (`native-builtins/src/phases_late.rs`,
the actively-dispatched path backing the synthetic
`java/lang/StackWalker$StackFrame`), with the parallel dormant
`populate_sfi` (`native-builtins/src/lang_stackwalker.rs`) given the same
treatment.

Both of those are still in the tree on `9ac1feffe` (`StackTraceEntry` still
carries `class_id`, `populate_stack_frame` still calls
`ctx.get_class_mirror(class_id)`), so the next session should start by
finding which frames still reach a null mirror despite that — candidates:
frames whose `class_id` is `None` at capture, `get_class_mirror` returning
null for a class mid-`<clinit>`, or a third `getDeclaringClass` implementation
that neither of those two paths covers.

## Reproduction

```bash
cd /data/data/wt-springsuite8b-20260726/apps/spring-suite-runner
CRATONVM_BIN=<binary> KRUN_STACK=1 ./one.sh \
  org.springframework.beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests
```

Deterministic — 14/14 fail every run, in ~1.2 s. Reproduces with the JIT on
and with `--nojit`, so it is a native/stack-capture defect, not a compilation
one.

## Provenance

Found while retiring the Spring JIT-ban inventory
(`docs/internal/jit-bans/spring-jit-bans-inventory-and-ban-lift-experiment-20260730.md`).
That doc's Part 2 first saw this NPE only with the javac-family JIT bans
lifted and recorded it as a possible ninth miscompile; Part 3 shows it
reproducing with those bans active and with the JIT off entirely, which rules
the JIT out.
