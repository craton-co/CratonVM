# `--jdk-only`: a `ClassCastException` (`Spliterators$ArraySpliterator` → `Object[]`) during JUnit launcher initialisation, once in a 733-class sweep — NOT attributed

| | |
|---|---|
| **Status** | OPEN, **one occurrence, unattributed, not reproduced.** Recorded so the next occurrence has something to be compared with. |
| **Found** | 2026-09-19, in the verification sweep of the merged tree for `heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md` (release binary, 3 classes in parallel, 240 s cap). |
| **Where** | `io.netty.buffer.ReadOnlyDirectByteBufferBufTest`, `--jdk-only`, 0.5 s after process start, before any test ran. It read as `HANG` (no `@@RESULT`); the earlier sweep had it at 60/60 in 17.9 s. |
| **Related** | `../../internal/jdk-only/junit-discovery-fails-when-run-concurrently-under-jdk-only-FIXED-20260919.md` — its last paragraph anticipated "a real GC-safety defect that the blocking-region fix only made rare". This may or may not be that. |

## The symptom

The first line of trouble is a class-initialiser failure, then the same exception uncaught:

```
WARN vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=org/junit/platform/launcher/TestIdentifier
     cause=java/lang/ClassCastException class java.util.Spliterators$ArraySpliterator cannot be cast to class [Ljava.lang.Object;
  [CLINIT-TRACE 14] TestIdentifier.<clinit> (TestIdentifier.java:51)
  [CLINIT-TRACE 15] java/io/ObjectStreamClass.lookup (ObjectStreamClass.java:227)
  ...                java/io/ClassCache$1.computeValue (ClassCache.java:75/78)
main-vm run() returned Err: Exception in thread "main" java/lang/ClassCastException:
  class java.util.Spliterators$ArraySpliterator cannot be cast to class [Ljava.lang.Object; ...
```

A value of the type an `Arrays.spliterator` call **returns** was handed to something that expected the **array it was
made from**. That is a type confusion between an allocation and its argument, not a logic error in JUnit.

**The printed Java stack is not one call chain.** `Preconditions.containsNoNullElements` does not call
`ReflectionUtils.findMethod`; `Arrays.stream` appears three times; the frames of `ClasspathAlignmentChecker.check` sit
between `Spliterators$ArraySpliterator.forEachRemaining` and `ReferencePipeline.forEach` in an order no call could
produce. Whatever built that trace was walking frames that were not in a consistent state. That is evidence of
*corruption of the VM's own frame record*, not merely of a wrong value in one frame.

## What is known about how often

| evidence | occurrences |
|---|---:|
| every earlier sweep log on the host that carries the signature (`grep 'ArraySpliterator cannot be cast'`, ~2,700 class runs across ten sweeps, both modes, earlier binaries) | 0 |
| the merged tree, `--jdk-only`, full 733 classes | **1** |
| the merged tree, default mode, first 95 classes of its sweep | 0 |
| 1,200 launcher initialisations of a nonexistent class, merged tree, `--jdk-only`, 3 in parallel | 0 |
| 180 launcher initialisations of a real 2-second class (`FlowControlHandlerTest`), same | 0 |
| 12 runs of the failing class alone, same | 0 |

## Why it is not attributed to the change that was being verified

The two changes under test are a JIT admission of a protected `ldc` and the retirement of the three int
`Preconditions.check*` and the sixteen `Unsafe.*Unaligned` natives. Reading the failing frames does **not** exclude
them: `Arrays.spliterator`/`Spliterators.spliterator` have no exception table, so the `ldc` admission has nothing to
act on *there*, but `Preconditions.check*` is called from nearly everywhere in the JDK (this run is reflection-heavy
`ObjectStreamClass` bytecode), and an admission that recompiles other methods on the path can change timing anywhere.
The merge that produced this binary also brought in `dev`'s JIT review round 9 (426 files), which is the larger and
less-inspected change. **Neither is a control** — no `dev`-only binary was run through the same sweep, and one event
in ~3,500 cannot discriminate.

## What would settle it

* A `dev`-only release binary through the same 733-class `--jdk-only` sweep (about 2.5 h) and the merged one again:
  a second event on either side attributes it, and a clean `dev`-only run is weak evidence for the merge.
* The scrambled trace is the best lead. The `[CLINIT-TRACE]` lines are already printed for the failing `<clinit>`;
  a run that also dumps the *interpreter frame vector* at the moment of the CCE would show whether the frame list or
  the operand stack was already inconsistent.
* `ObjectStreamClass.lookup` of `TestIdentifier` is the workload. It is reflection-heavy real JDK bytecode
  (`Class.getDeclaredMethods`, `AccessController`, `Modifier`) that the default mode replaces with natives, so it is
  a `--jdk-only`-shaped path. A loop that does only `ObjectStreamClass.lookup(X.class)` for a `Serializable` `X` with a
  `serialVersionUID`, from several threads while a GC is forced, is the cheapest targeted stressor.
