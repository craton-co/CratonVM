# Mockito inline mock maker — ASM's `ClassReader` NPEs near an exception-table boundary — RESOLVED

**Status: FIXED (2026-08-05).** The class bytes CratonVM hands to a
`ClassFileTransformer` during retransformation were never malformed. It is the
recycled-`JitInvokeInfo` dispatch aliasing fixed by `383e7f5cf`; see
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and
`mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md` for the
cluster-wide validation.

## Original symptom (as filed)

`Mockito.mock(SomeConcreteClass.class)` through the **inline** mock maker failed
with `MockitoException: Could not modify all classes […]` →
`IllegalStateException: Byte Buddy could not instrument all classes within the
mock's type hierarchy` → `NullPointerException` at
`MethodVisitor.visitVarInsn` ← `ClassReader.readCode` ←
`Advice$Dispatcher$Inlining$Resolved$AdviceMethodInliner.apply` ←
`AdviceVisitor.onAfterExceptionTable` ←
`ExceptionTableSensitiveMethodVisitor.considerEndOfExceptionTable`.

Observed for `org.springframework.scheduling.concurrent.ThreadPoolTaskExecutor`
(`TaskExecutorMetricsAutoConfigurationTests`, 1/9) and with the same shape at
`visitEnd`/`CodeTranslationVisitor` for
`io.micrometer.observation.ObservationRegistry$ObservationConfig`
(`ObservationHandlerGroupTests`, 1/5). HotSpot passed both at 9/9 and 5/5.

## Root cause

`383e7f5cf`. Per-thread dispatch memos in `vm/src/jit/helpers.rs` are keyed on
`(vm_identity, JitInvokeInfo pointer)`; the boxes are freed with their
`CompiledMethod` and the allocator re-issues the address to the next compile, so
a compiled call site inherits the previous site's resolution. `NATIVE_SITE_CACHE`
holds a resolved leaf-native callback — the aliased site calls a different
native and returns whatever it returns.

ASM was not parsing malformed bytes. ASM's `ClassReader` is itself
native-call-dense while walking a `Code` attribute (`readUTF8`, `readConst`,
array and buffer accessors), and one of those calls answering from another
site's native returns the wrong value at whatever offset the walk had reached —
so the fault lands wherever the walk happened to be. The exception table is not
special; it is where two runs happened to stop.

## Why the filed hypothesis was wrong, and why its diagnostic would not have found it

The page reasoned that "a well-formed class from `javac` should never trip ASM
this way", concluded the byte array CratonVM materializes for a retransform
request must be malformed, and prescribed:

```
grep -rn "retransform\|ClassFileTransformer\|redefine_class" vm/src classloading/src native-builtins/src --include=*.rs
```

plus a byte-level comparison of the transformer's input against `javap -c`.

**That diagnostic would have come back clean and read as a dead end.** The bytes
are correct — `vm/src/runtime/instrument.rs::original_class_bytes` returns the
`ClassManager`'s cached original bytes, and its classpath fallback already
verifies `this_class` before seeding a retransform. Dumping and diffing them
against `javap` produces a match, at which point the only remaining move is to
doubt the dump.

The tell that was already on the page: **the crash site moved between the two
affected classes** (`visitVarInsn` vs `visitEnd`), and both are ASM's own
walk over bytes ASM itself had just been handed successfully enough to reach a
method body. A malformed constant pool or `Code` attribute fails at a fixed
structural point; a wrong *value* delivered mid-walk fails wherever the walk is.

## Validation

See `mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md` for the
cluster measurement (13 classes across four binaries, isolation and concurrent
arms). Both classes on this page are in that set.

Pre-fix (`1078f6f05c`, no `383e7f5cf`), concurrent arm,
`TaskExecutorMetricsAutoConfigurationTests` was the most reproducible class in
the cluster — **4 of 6 rounds bad** — and every round failed differently, which
is the aliasing's signature rather than a fixed malformed-bytes defect:

| Round | Face |
|---|---|
| r1 | `BeanInitializationException … IllegalArgumentException: RepeatableContainers must not be null` |
| r2 | `AnnotationConfigurationException: Misconfigured aliases … Different @AliasFor mirror values` |
| r3 | `NPE: Cannot invoke "String.isEmpty()" because "packageName" is null`, then `FileNotFoundException: class path resource [null.class] cannot be opened` |
| r6 | `MockitoException: Could not modify all classes [interface org.springframework.core.task.TaskExecutor, …]` → `IllegalStateException` → `IllegalArgumentException: value null` |

`ObservationHandlerGroupTests` did not reproduce in 6 concurrent rounds
(0/6 pre-fix), so its retirement rests on the shared mechanism plus the clean
fixed-tip rounds rather than on its own before/after pair. Recorded rather than
smoothed over.

On every arm carrying `383e7f5cf` both classes are 9/9 and 5/5, and none of the
strings above appears in their output.

## Affected classes

- `module/spring-boot-micrometer-metrics` — `org.springframework.boot.micrometer.metrics.autoconfigure.task.TaskExecutorMetricsAutoConfigurationTests`
- `module/spring-boot-micrometer-observation` — `org.springframework.boot.micrometer.observation.autoconfigure.ObservationHandlerGroupTests`
