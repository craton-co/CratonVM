# Mockito's inline mock maker: bytecode CratonVM hands back for JVMTI-style class retransformation crashes ASM's `ClassReader` with an NPE near an exception-table boundary

**Status: OPEN — found 2026-08-05**

## Symptom

`Mockito.mock(SomeConcreteClass.class)` (routed through Mockito's **inline**
mock maker's `InlineBytecodeGenerator`, i.e. the class actually gets
retransformed in place via `java.lang.instrument`, as opposed to Byte
Buddy's plain subclassing path used for interfaces) fails with:

```
org.mockito.exceptions.base.MockitoException: Mockito cannot mock this class: class org.springframework.scheduling.concurrent.ThreadPoolTaskExecutor.
Underlying exception : org.mockito.exceptions.base.MockitoException: Could not modify all classes [... full type hierarchy ...]
Caused by: java.lang.IllegalStateException: Byte Buddy could not instrument all classes within the mock's type hierarchy
	at org.mockito.internal.creation.bytebuddy.InlineBytecodeGenerator.triggerRetransformation(InlineBytecodeGenerator.java:310)
Caused by: java.lang.NullPointerException
	at net.bytebuddy.jar.asm.MethodVisitor.visitVarInsn(MethodVisitor.java:372)
	at net.bytebuddy.jar.asm.ClassReader.readCode(ClassReader.java:2236)
	at net.bytebuddy.jar.asm.ClassReader.readMethod(ClassReader.java:1512)
	at net.bytebuddy.asm.Advice$Dispatcher$Inlining$Resolved$AdviceMethodInliner.apply(Advice.java:9671)
	at net.bytebuddy.asm.Advice$AdviceVisitor.onAfterExceptionTable(Advice.java:11978)
	at net.bytebuddy.utility.visitor.ExceptionTableSensitiveMethodVisitor.considerEndOfExceptionTable(ExceptionTableSensitiveMethodVisitor.java:50)
	at net.bytebuddy.jar.asm.ClassReader.readCode(ClassReader.java:2056)
	at net.bytebuddy.internal.creation.bytebuddy.InlineBytecodeGenerator.transform(InlineBytecodeGenerator.java:427)
	at net.bytebuddy.internal.creation.bytebuddy.InlineBytecodeGenerator.triggerRetransformation(InlineBytecodeGenerator.java:306)
```

Also observed with the identical `visitEnd`/`Advice$Dispatcher$Inlining$CodeTranslationVisitor`
crash shape (`ObservationHandlerGroupTests`) for
`io.micrometer.observation.ObservationRegistry$ObservationConfig`. HotSpot
passes both classes at 9/9 and 5/5 respectively
(`hotspot-baseline-latest.tsv`).

ASM's own `ClassReader` is crashing while parsing bytecode **CratonVM
supplied** (via the JDK's `Instrumentation`/retransform machinery that
Mockito's self-attached agent drives) — the failing frame is always
`ClassReader.readCode` walking a method's `Code` attribute right at or just
after its exception table (`considerEndOfExceptionTable` →
`AdviceMethodInliner.apply` → `readCode`/`readMethod` → a raw-opcode visit
NPEs on a null operand). This means the class bytes CratonVM returns for
retransformation are malformed in a way that only shows up when a
downstream consumer (ASM, here) walks an exception-table-adjacent region of
a method's `Code` attribute — a well-formed class from `javac` should never
trip ASM this way.

## Root cause

Not yet pinned to a specific Rust site — **needs further investigation**.
The failure is specific to CratonVM's response to a `retransformClasses`-style
request (Mockito's `InlineBytecodeGenerator` obtains the class bytes to
patch via `java.lang.instrument.ClassFileTransformer`/`Instrumentation`, not
by loading the resource from disk), so the bug is most plausibly in
whatever CratonVM code path materializes the byte[] handed to a registered
`ClassFileTransformer` during a retransform request (as opposed to the
original-load `.class` bytes, which are presumably fine since the classes
load and run normally otherwise). Start with:

```
grep -rn "retransform\|ClassFileTransformer\|redefine_class" vm/src classloading/src native-builtins/src --include=*.rs
```
and compare the byte[] CratonVM hands to the transformer against `javap -c`
of the same class/method around its exception table, for one of the two
confirmed-failing classes (`ThreadPoolTaskExecutor` or
`ObservationRegistry$ObservationConfig`).

## Affected classes
- `spring-boot-micrometer-metrics` — `org.springframework.boot.micrometer.metrics.autoconfigure.task.TaskExecutorMetricsAutoConfigurationTests` (1/9 failed: `threadPoolTaskExecutorWithNoTaskExecutorIsIgnored`, mocking `ThreadPoolTaskExecutor`)
- `spring-boot-micrometer-observation` — `org.springframework.boot.micrometer.observation.autoconfigure.ObservationHandlerGroupTests` (1/5 failed: `registerMembersRegistersUsingFirstMatchingCompositeObservationHandler`, mocking `ObservationRegistry$ObservationConfig`)
