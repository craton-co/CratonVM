# StructuredTaskScope is a JEP 505 interface now, and the interesting defect was not the stale names

**Status: the two dead JDK-21 class names are REMOVED and the JEP 505 surface is
registered, both in `native-builtins/src/phases_late/concurrent.rs`, on the
synthetic-JDK path only. The defect that actually breaks JDK 25 code is in a
different file and is written out below under
[Out-of-file patch (not applied)](#out-of-file-patch-not-applied).** Nothing was
rebuilt in this lane: no claim is made that the edited source compiles or that
the synthetic surface behaves. Every measurement below was taken with HotSpot
Adoptium 25.0.3.9 and with the pre-change binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-11 19:41), and
each is reproducible with `probes/StructuredTaskScopeProbe.java`.

## What this was supposed to be, and what it turned out to be

docs/known-issues/jdk-only/W7-14-fjp-common-factory-bound-by-name.md swept this
file for hard-coded JDK-internal names and left two rows open:

> `StructuredTaskScope$ShutdownOnSuccess` … **rotted, same JDK-21 era — not fixed**
> `StructuredTaskScope$ShutdownOnFailure` … **rotted, same JDK-21 era — not fixed**

with the reasoning that "the replacement is a different API shape, not a rename",
and the honest caveat that their inertness was inferred rather than measured:
*"that is consistent with inertness but does not prove it, and no probe was run
for them."*

The probe has now been run. Both rows are confirmed dead, and they are dead
twice over. But the run also found something the sweep could not: **in
`--real-jdk` and `--jdk-only`, CratonVM already runs the whole JEP 505 API out of
real JDK bytecode, and every one of its 15 divergences from HotSpot traces to a
single defect in `native-builtins/src/shared_secrets_bridge.rs` that has nothing
to do with names.** `join()` does not wait.

## 1. The measured JDK 25 surface

`javap` on Adoptium 25.0.3.9, `java.base`:

```java
public sealed interface StructuredTaskScope<T,R> extends AutoCloseable
        permits StructuredTaskScopeImpl {
    static <T,R> StructuredTaskScope<T,R> open(Joiner<? super T,? extends R>,
                                               Function<Configuration,Configuration>);
    static <T,R> StructuredTaskScope<T,R> open(Joiner<? super T,? extends R>);
    static <T>   StructuredTaskScope<T,Void> open();
    <U extends T> Subtask<U> fork(Callable<? extends U> task);
    <U extends T> Subtask<U> fork(Runnable task);
    R       join() throws InterruptedException;
    boolean isCancelled();
    void    close();
}
```

`STS.constructors=0` — there is no constructor to call, and `STS.isSealed=true`,
so there is no subclass to write either. The nested types:

| nested type | JDK 25 | shape |
|---|---|---|
| `$Joiner` | present | `public interface`, NOT sealed — user code may implement it |
| `$Subtask` | present | `public sealed interface extends Supplier<T>` |
| `$Subtask$State` | present | `public final` enum: `UNAVAILABLE, SUCCESS, FAILED` |
| `$Configuration` | present | `public sealed interface`, three withers |
| `$FailedException` | present | `public final class extends RuntimeException` |
| `$TimeoutException` | present | `public final class extends RuntimeException` |
| **`$ShutdownOnSuccess`** | **ABSENT** | deleted by JEP 505 |
| **`$ShutdownOnFailure`** | **ABSENT** | deleted by JEP 505 |
| **`$Config`** | **ABSENT** | never existed in any JDK; a CratonVM invention |

`Joiner`'s five static factories, with the implementation class each returns on
HotSpot 25 — this is the table that replaces the deleted subclasses:

```
Joiner.awaitAll                  = java.util.concurrent.StructuredTaskScope$Joiner$1
Joiner.awaitAllSuccessfulOrThrow = java.util.concurrent.Joiners$AwaitSuccessful
Joiner.allSuccessfulOrThrow      = java.util.concurrent.Joiners$AllSuccessful
Joiner.anySuccessfulResultOrThrow= java.util.concurrent.Joiners$AnySuccessful
Joiner.allUntil(Predicate)       = java.util.concurrent.Joiners$AllSubtasks
```

Each call returns a **fresh** instance (`Joiner.awaitAll.sameInstanceTwice=false`),
which matters because a `Joiner` is stateful. `Joiner` also carries two `default`
methods, `onFork(Subtask)boolean` and `onComplete(Subtask)boolean`, and one
abstract `result()`.

The mapping from the deleted classes, which is what makes this a feature and not
a rename:

| JDK 21-24 | JDK 25 |
|---|---|
| `new StructuredTaskScope.ShutdownOnSuccess<T>()` | `StructuredTaskScope.open(Joiner.anySuccessfulResultOrThrow())` |
| `new StructuredTaskScope.ShutdownOnFailure()` | `StructuredTaskScope.open(Joiner.awaitAllSuccessfulOrThrow())` |
| `scope.join()` returning `this` | `scope.join()` returning `R`, the joiner's result |
| `scope.throwIfFailed()` / `scope.result()` | folded into `join()`'s throw / return |
| `scope.isShutdown()` | `scope.isCancelled()` |
| `scope.joinUntil(Instant)` | `Configuration.withTimeout(Duration)` |
| `new StructuredTaskScope(name, threadFactory)` | `Configuration.withName` / `.withThreadFactory` |

## 2. The preview gating, measured

This is a **fifth-preview** API and it is gated in three separable places. All
four commands below were run on Adoptium 25.0.3.9.

```
$ javac P.java
P.java:1: error: StructuredTaskScope is a preview API and is disabled by default.
  (use --enable-preview to enable preview APIs)

$ javac --release 25 --enable-preview P.java
Note: P.java uses preview features of Java SE 25.

$ javap -v P | grep -i version
  minor version: 65535
  major version: 69

$ java -cp . P
Error: LinkageError occurred while loading main class P
  java.lang.UnsupportedClassVersionError: Preview features are not enabled for P
  (class file version 69.65535). Try running with '--enable-preview'

$ java --enable-preview -cp . P
t=42
```

Three facts worth separating, because they are easy to conflate:

1. **The JDK's own `StructuredTaskScope.class` is NOT preview-flagged.**
   `magic=cafebabe major=69 minor=0 preview=false`, same for
   `StructuredTaskScopeImpl.class`. Preview-ness of the API is carried by the
   `@PreviewFeature(feature = STRUCTURED_CONCURRENCY)` annotation on the source
   (seven occurrences in `StructuredTaskScope.java`) and enforced by **javac**.
2. **The user's class file IS preview-flagged**, minor `0xFFFF`, and that is what
   HotSpot refuses to load without `--enable-preview`.
3. **Reflection is not gated at all.**
   `Class.forName("java.util.concurrent.StructuredTaskScope")` and reflective
   `open`/`fork`/`join` all succeed on plain `java` with no flags. This is why
   `probes/StructuredTaskScopeProbe.java` is reflection-only — see §4.

### CratonVM's preview gating: there is none, in both directions

Measured on the pre-change binary:

```
$ cratonvm --enable-preview -cp . P
error: unexpected argument '--enable-preview' found

$ cratonvm --real-jdk -cp . P        # P.class is 69.65535
(runs; no UnsupportedClassVersionError)
```

So CratonVM **accepts a preview class file that HotSpot rejects**, and has no
flag with which to accept it deliberately. That is a divergence in its own right
— a JVM that runs preview bytecode unconditionally will happily run code the
target JDK would refuse — but it is in the class-file parser, not in this file,
and it is not what breaks StructuredTaskScope. Recorded, not fixed here.

The practical consequence for anyone writing a vector: **do not name
`StructuredTaskScope` in Java source that has to run on both VMs.** The class
file will be `69.65535`, HotSpot will refuse it without `--enable-preview`,
CratonVM has no such flag, and a differential with only one working arm is not a
differential. Use reflection.

## 3. The two dead JDK-21 names, measured rather than inferred

`probes/StructuredTaskScopeProbe.java`, section `deadJdk21Names`, is byte-identical
on HotSpot 25, `cratonvm --real-jdk` and `cratonvm --jdk-only`:

```
forName.java.util.concurrent.StructuredTaskScope$ShutdownOnSuccess=java.lang.ClassNotFoundException:...
forName.java.util.concurrent.StructuredTaskScope$ShutdownOnFailure=java.lang.ClassNotFoundException:...
forName.java.util.concurrent.StructuredTaskScope$Config=java.lang.ClassNotFoundException:...
forName.jdk.incubator.concurrent.StructuredTaskScope=java.lang.ClassNotFoundException:...
forName.jdk.incubator.concurrent.StructuredTaskScope$Subtask=java.lang.ClassNotFoundException:...
forName.jdk.incubator.concurrent.StructuredTaskScope$ShutdownOnSuccess=java.lang.ClassNotFoundException:...
forName.jdk.incubator.concurrent.StructuredTaskScope$ShutdownOnFailure=java.lang.ClassNotFoundException:...
STS.declaredClasses=...$Configuration,...$FailedException,...$Joiner,...$Subtask,...$TimeoutException
```

**They were dead twice over, and the second reason is the interesting one.** The
sweep assumed a registration on an undeclared class is inert because the class
never loads. True in real-JDK modes. But in **synthetic** mode the class *is*
fabricated (`classloading/src/class_manager.rs` has rows for both, at
`instance_fields(8)`), so the natives would bind — except that they still could
not run, because:

* `register_p67_structured_task_scope` is reachable only from
  `register_synthetic_overrides` (docs/architecture/natives-over-real-jdk-classes.md §2),
  so it does not register at all in `--real-jdk`/`--jdk-only`; and
* in synthetic mode, `native-builtins/src/jdk25_concurrency.rs`'s
  `register_jdk25_concurrency_natives` re-registers **every one of the same
  triples** and is called by `register_synthetic_overrides` *after* this
  registrar (`lib.rs:23602` vs `lib.rs:23781`), and `register()` is
  last-registration-wins (§3 of the same document).

I checked the two triple sets against each other. The only triples this file
contributed that `jdk25_concurrency.rs` did not overwrite were
`ShutdownOnSuccess.join()LShutdownOnSuccess;` and its `joinUntil` twin, plus the
same pair on `ShutdownOnFailure` — four covariant-return methods, on two classes
JDK 25 does not declare. **So the deletion's behavioural effect is exactly
zero**, and that is a measured claim about registration order rather than a hope.

That is the shape docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md
censuses, and the shape
docs/known-issues/jdk-only/W7-14-fjp-common-factory-bound-by-name.md named for
`SynchronousQueue$Itr`: coverage that is not there.

### The `jdk.incubator.concurrent.*` block is the same defect, one release older

The same function still registers ~30 natives against
`jdk/incubator/concurrent/StructuredTaskScope` and three nested types. No JDK 25
declares that package (four `ClassNotFoundException` rows above), and
`class_manager.rs` has no fabrication row for it either, so it is inert in both
modes. **Not deleted here**, for the reason W7-14 gave for `SynchronousQueue$Itr`:
it belongs to the never-shipped-registrar census, which already counts this
registrar, not to a lane fixing the JDK 25 shape. It now carries an in-file
comment stating the measured verdict so the next reader does not re-derive it.

## 3.5. Reproducing every number above

No golden file is checked in, deliberately: a frozen transcript of a preview API
rots at the next JDK and rots silently, and the oracle is three seconds away.

```sh
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
"$JDK/bin/javac" -d /tmp/sts probes/StructuredTaskScopeProbe.java

# oracle — take it three times; it must be byte-identical to itself
for i in 1 2 3; do "$JDK/bin/java" -cp /tmp/sts StructuredTaskScopeProbe > /tmp/hs$i.txt; done
diff /tmp/hs1.txt /tmp/hs2.txt && diff /tmp/hs2.txt /tmp/hs3.txt

# the two CratonVM arms (strip the ANSI-coloured tracing lines first)
cratonvm --real-jdk -cp /tmp/sts StructuredTaskScopeProbe
cratonvm --jdk-only --jdk-only-report /tmp/sts.json -cp /tmp/sts StructuredTaskScopeProbe
```

**Run the CratonVM arm three times as well.** The single most informative result
in this record is not any one line, it is that HotSpot's transcript is stable and
CratonVM's is not — see §5.

## 4. The probe, and why it is reflection-only

`probes/StructuredTaskScopeProbe.java`, 147 lines of output, byte-identical
across three consecutive HotSpot runs.

Reflection is forced by §2: naming the type in source produces a `69.65535`
class file that only one of the two VMs will load. Reflection carries no preview
bit, so one class file runs on all three arms and the preview gating becomes a
line the probe *prints* rather than a precondition for taking the measurement.

Four disciplines, three of them the ones
`probes/ShadowDifferentialProbe.java` states and one this API forces:

* **FENCED** — a section that throws prints one `SECTION-DIED.x` line. A
  truncated transcript reads exactly like a short clean run.
* **VALUES, NOT VERDICTS** — `subtask.state.afterJoin=UNAVAILABLE` diffs; `ok`
  does not.
* **BOUNDED**, and here it is load-bearing rather than hygienic. `join()` is a
  blocking wait on threads the scope started, so a wrong implementation hangs
  rather than fails. Every section runs on its own **daemon** thread joined with
  an 8s timeout; an overrun prints `SECTION-HUNG.x` and the transcript continues.
* **OWNER-THREAD SAFE**, which falls out of the previous point for free —
  `fork`/`join`/`close` must all happen on the thread that opened the scope
  (`WrongThreadException` otherwise), and one thread per section gives that.

Two lines were measured and then deliberately removed. `state()` before `join()`
is a **race on a correct VM** and a fixed answer on a broken one, so it would
flake on HotSpot and read clean on CratonVM — the wrong way round.
`PreviewFeatures.isEnabled`'s refusal message embeds the unnamed module's
identity hash, which changes every run; it prints its throwable *type* only.
With those two settled the oracle is stable.

## 5. What CratonVM does today

### `--real-jdk` and `--jdk-only`: real bytecode serves the entire API

```
open.scopeClass    = java.util.concurrent.StructuredTaskScopeImpl
open.subtaskClass  = java.util.concurrent.StructuredTaskScopeImpl$SubtaskImpl
Joiner.awaitAllSuccessfulOrThrow = java.util.concurrent.Joiners$AwaitSuccessful
```

identical to HotSpot. `--jdk-only --jdk-only-report` over the full probe:

```json
"counts": {"boot_image_classes": 542, "application_classes": 3,
           "generated_classes": 8, "compatibility_classes": 0,
           "bridge_invocations": 2293, "intrinsic_invocations": 832,
           "synthetic_stub_invocations": 0}
```

**`compatibility_classes: 0`, `synthetic_stub_invocations: 0`, and not one
StructuredTaskScope violation in 1,454 recorded violations.** No CratonVM native
participates. Whole sections match HotSpot line for line: the declared surface,
the preview gating, the dead-name census, the state machine (all nine
`IllegalStateException` messages, verbatim), owner-thread confinement (three
`WrongThreadException:Current thread not owner`), and the Subtask carrier.

### The 15 lines that diverge, and they are one defect

```
                                          HotSpot 25            cratonvm --real-jdk
joinWaits.taskFinishedWhenJoinReturned     true                  false
joinWaits.joinBlockedAtLeast200ms          true                  false
joinWaits.subtask.state.afterJoin          SUCCESS               UNAVAILABLE
joinWaits.subtask.get                      done                  ISE:Result is unavailable...
awaitAllSuccessful.fail.join               FailedException:      no-throw
                                             ISE: boom-c
awaitAllSuccessful.fail.isCancelled        true                  false
awaitAllSuccessful.fail.bad.state          FAILED                UNAVAILABLE
anySuccessful.join.result                  fast                  FailedException:
                                                                   NoSuchElementException:
                                                                   No subtasks completed
allUntil.predicate.invocations             1                     0
forkRunnable.ran                           1                     0
configuration.subtask.get                  platform              ISE:Result is unavailable...
configuration.default.subtaskThreadKind    virtual               ISE:Result is unavailable...
timeout.join                               TimeoutException      no-throw
timeout.isCancelled                        true                  false
```

Every one of those is downstream of one fact: **`join()` returns before the
forked subtask has run.** The strongest single statement of it is not in the
table but in the runs themselves — **HotSpot's transcript is byte-identical
across three consecutive runs, and CratonVM's is not.** Three `--real-jdk` runs
differed from each other on `awaitAllSuccessful.ok.get`,
`joinWaits.joinReturnedBeforeTaskStarted`, `awaitAll.bad.state`,
`awaitAll.bad.exception`, `forkRunnable.state` and `subtask.b.get`. A `join()`
that does not wait turns the entire API into a race, and the tasks that happen to
win it are the reason a shallower probe would report this surface as working.

### Root cause, isolated to one call

Not inferred — bisected with three throwaway probes:

```
                                              HotSpot 25   cratonvm --real-jdk
Thread.ofVirtual().start(r) then join()        ran=1        ran=1
Thread.ofVirtual().factory().newThread(r)      ran=1        ran=1
flock.threadCount immediately after fork()     1            0
flock.containsThread(currentThread) in task    true         false
subtask state 700ms after fork, before join    SUCCESS      SUCCESS
```

The thread is created, started, and runs to completion. What never happens is
its **registration with the scope's `ThreadFlock`**. `StructuredTaskScopeImpl.join()`
is `flock.awaitAll()`, and `ThreadFlock.awaitAll()` opens with

```java
if (threadCount == 0) return true;
```

so with `threadCount` stuck at 0 it returns instantly and `join()` waits for
nothing. The binding is supposed to happen in `ThreadFlock.start`:

```java
public Thread start(Thread thread) {
    ensureOwnerOrContainsThread();
    JLA.start(thread, container);      // <- the container is the whole point
    return thread;
}
```

and `JavaLangAccess.start(Thread, ThreadContainer)` is shadowed by a CratonVM
native — `native-builtins/src/shared_secrets_bridge.rs`,
`jla_start_in_container`:

```rust
    ctx.invoke_virtual(thread_obj, "start", "()V", &[])?;
```

`args[2]`, the `ThreadContainer`, is read in the doc comment's parameter list and
never touched. The comment says so outright — *"CratonVM does not model thread
containers (no structured-concurrency introspection), so … this is a behavioral
passthrough: just start the thread for real."* That was true when it was written
for Jetty's thread pool, where nothing reads the container. JEP 505's `join()`
reads it, and reads it as a **wait condition**.

The real `Thread.start(ThreadContainer)` (JDK 25, `Thread.java:1426`) does three
things this passthrough does one of:

```java
    setThreadContainer(container);
    container.add(this);        // -> ThreadFlock.onStart -> threadCount++
    start0();
```

with the matching `container.remove(this)` in `Thread.exit()` (`Thread.java:1517`)
doing the decrement and the wakeup.

### Why the `--jdk-only` arm has one extra failure

```
timeout.threw=java.lang.NoClassDefFoundError:
  java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory
```

`StructuredTaskScopeImpl.scheduleTimeout` calls `ForkJoinPool.commonPool()`.
That is **not a finding of this lane** — it is exactly
docs/known-issues/jdk-only/W7-14-fjp-common-factory-bound-by-name.md, and it was
fixed on `dev` at 2026-08-11 19:56 while the binary measured here was built at
19:41. It is reported so the transcript is not read as a second defect, and so a
re-run against a current binary can confirm that line moves.

## 6. What this lane changed, and what it deliberately did not

In `native-builtins/src/phases_late/concurrent.rs`, synthetic-JDK path only:

**Deleted** — 20 registrations on `$ShutdownOnSuccess`/`$ShutdownOnFailure`
(eight apiece from the shared `j25_register_scope_common`, plus `result`,
`exception` and two `throwIfFailed` overloads), and 11 JDK-21-shaped triples on
`StructuredTaskScope` itself: `<init>` ×2, `fork(Callable)`,
`join()LStructuredTaskScope;`, `joinUntil(Instant)`, `shutdown`, `close`,
`isShutdown`, and the three `Subtask` accessors. Effect: zero, per §3.

**Added** — seven triples, chosen by one rule: *nothing else in the tree
registers them*, so none can be silently overwritten by the later registrar and
none is a second body for a method that already has one.

| triple | why it is a gap |
|---|---|
| `StructuredTaskScope.join()Ljava/lang/Object;` | JEP 505 changed the return type from `this` to `R`, so javac emits this descriptor and the surviving `()LStructuredTaskScope;` registration is bound to a triple no JDK 25 call site can produce |
| `StructuredTaskScope.isCancelled()Z` | replaced `isShutdown()Z`; also true after `close()`, which the probe pins |
| `StructuredTaskScope.fork(Ljava/lang/Runnable;)L…$Subtask;` | new in JEP 505; its Subtask's `get()` answers **null as a result**, not as an absence |
| `StructuredTaskScope.open(Joiner,Function)L…;` | applies the config Function — the probe's `configuration.applied` is what catches an implementation that skips it |
| `Joiner.allUntil(Predicate)L…$Joiner;` | the fifth factory; the other four are registered elsewhere |
| `Joiner.onFork(L…$Subtask;)Z` | a `default` interface method with no body in synthetic mode |
| `$Configuration.withName/withThreadFactory/withTimeout` | the name JDK 25 declares; the existing `$Config` is a name no JDK ever shipped |

**Nothing was added to the real-JDK path, on purpose.** §5 measured that real
bytecode serves this API end to end there with zero fabrication. A native there
would be a shadow over working JDK code, which is the population
docs/architecture/natives-over-real-jdk-classes.md §1 and the shadow-differential
work exist to shrink, not grow. `Compatible` mode is byte-for-byte unchanged
structurally, not by inspection: the registrar is unreachable from the real-JDK
boot path.

**Fork stays synchronous** in the synthetic model. The task runs on the forking
thread before `fork` returns, so `join()` has nothing left to wait for. That is
deliberate: a structured-concurrency API that hangs is worse than one that fails,
and this tree already carries eight recorded hangs from three causes, one of them
`ForkJoinTask.invokeAll` blocking in `awaitDone` for a forked sibling no worker
would run. No code path added here has an unbounded wait. The cost, stated
plainly: `isCancelled()` can never be observed mid-flight, and there is no
parallelism.

## Out-of-file patch (not applied)

### A. The defect: `JavaLangAccess.start` drops the ThreadContainer

`native-builtins/src/shared_secrets_bridge.rs`. Two registrations, both in
`register_java_lang_access`, one on `java/lang/System$1` and one on the
`jdk/internal/access/JavaLangAccess` interface, both pointing at
`jla_start_in_container`.

**Preferred patch: delete both registrations and the function.** `javap -p
java.lang.System$1` on JDK 25 shows the method has a real body —

```
class java.lang.System$1 implements jdk.internal.access.JavaLangAccess {
  public void start(java.lang.Thread, jdk.internal.vm.ThreadContainer);
```

— and that body is one line, `thread.start(container)`. Removing the shadow lets
it run, which reaches the real `Thread.start(ThreadContainer)` and therefore
`setThreadContainer` + `container.add(this)` + `start0()`. **`start0()V` is
already registered on the real-JDK path** (`native-builtins/src/lib.rs`,
`register_essential_natives_with_shims`, → `lang_system::native_thread_start0`),
so the spawn still happens through the VM's own machinery. This is the
let-real-bytecode-run answer and it is why it is preferred over patching the
body.

**Fallback, if deleting both re-breaks what the bridge was written for.** Its doc
comment attributes it to a `NoSuchMethodError` that aborted Jetty's thread pool
at startup. If that returns, keep the registration but stop dropping the
argument:

```rust
fn jla_start_in_container(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let thread_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let container = args.get(2).copied().unwrap_or(Value::Object(None));
    // `Thread.start(ThreadContainer)` is a DIFFERENT triple from `Thread.start()V`
    // (which this file's `native_thread_start0` shadows), so this reaches the
    // JDK's own body: setThreadContainer + container.add(this) + start0(). The
    // container registration is not introspection — `ThreadFlock.awaitAll()`
    // returns immediately while `threadCount == 0`, so dropping the container is
    // what makes `StructuredTaskScope.join()` not wait.
    ctx.invoke_virtual(
        thread_obj,
        "start",
        "(Ljdk/internal/vm/ThreadContainer;)V",
        &[container],
    )?;
    Ok(None)
}
```

**Caveat, and it is the reason this is a patch and not a fix.** `VirtualThread`
overrides `start(ThreadContainer)` with a body that ends in
`externalSubmitRunContinuationOrThrow()` rather than `start0()`, and the default
`StructuredTaskScope` Configuration is `Thread.ofVirtual().factory()`. Plain
`Thread.ofVirtual().start(r)` was measured working on this binary, and JDK 25's
`VirtualThread.start()` is itself `start(ThreadContainers.root())`, so that path
is already exercised — but it is exercised through whatever CratonVM does with
`Thread.start()V`, and this patch would route it through the JDK body instead.
**Whoever applies this must re-run `probes/StructuredTaskScopeProbe` on both
arms and check `configuration.default.subtaskThreadKind=virtual` as well as the
`joinWaits.*` block.**

**Falsifier.** After a rebuild, under `--real-jdk` on JDK 25:

* `joinWaits.taskFinishedWhenJoinReturned` should become `true`,
  `joinWaits.joinBlockedAtLeast200ms` `true`, `joinWaits.subtask.get` `done`.
* Three consecutive runs of the probe should become byte-identical to each other,
  which they are not today. If they diverge anywhere, the container is being
  bound on some paths and not others.
* `timeout.join` should become `StructuredTaskScope$TimeoutException` and
  `awaitAllSuccessful.fail.join` should become
  `FailedException:java.lang.IllegalStateException: boom-c`.
* If instead `join()` now **hangs**, the `add` half landed and the `remove` half
  did not: `Thread.exit()`'s `container.remove(this)` is the decrement, and a VM
  that spawns threads without running `Thread.exit()` will increment a counter
  nothing ever decrements. That is the failure mode to watch for, and it is worse
  than today's — check it before landing.

### B. `jdk25_concurrency.rs` still models the JDK-21 shape

Out of scope for this lane and listed so it is not rediscovered.
`native-builtins/src/jdk25_concurrency.rs` registers, in synthetic mode:
`<init>` ×3, `joinUntil`, `shutdown`, `isShutdown`, `join()LStructuredTaskScope;`,
the full `ShutdownOnSuccess`/`ShutdownOnFailure` surface (~24 triples on two
classes JDK 25 does not declare), a `$Config` carrier under a name no JDK ever
shipped, and a `Joiner.policy()I` method the JDK does not declare. It also keeps
the scope→joiner association in two `HashMap`s keyed on
`ObjectRef::as_ptr() as usize` (`SCOPE_OWNERS`, `SCOPE_JOINERS`) — the
address-keyed side table whose recycled-address hazard this tree has recorded
before. Since it runs last, it owns every triple it shares with this file.

### C. Two joiners cannot be served by the 8-slot layout

`Joiner.allSuccessfulOrThrow()` and `Joiner.allUntil(Predicate)` both specify
`join()` returning `Stream<Subtask<T>>`. The synthetic scope has eight slots,
pinned by `classloading/src/class_manager.rs` (`instance_fields(8)`) and shared
with `jdk25_concurrency.rs`, and none of them can hold the subtask list such a
stream is built from — a ninth index would be heap corruption rather than a wrong
answer (docs/architecture/natives-over-real-jdk-classes.md §5). `join()` therefore
answers Void for them, which is right for `awaitAllSuccessfulOrThrow` and
`awaitAll` and wrong for these two. **Not faked**: a fabricated empty Stream reads
as a pass to every caller that only iterates, which is the exact shape
`probes/JdkOnlyCollectionViewProbe` exists because of.

`allUntil`'s Predicate is likewise not consulted — the four-slot `$Joiner`
carrier is fully used by `jdk25_concurrency.rs`, and an address-keyed side table
is the hazard in (B). It mints an `awaitAll` joiner instead, i.e. `allUntil` with
a predicate that never fires: over-waits rather than under-waits, which is the
safe direction when every subtask has already run.

The patch for both is one change: widen
`java/util/concurrent/StructuredTaskScope` to nine `instance_fields` in
`class_manager.rs`, use slot 8 for a subtask list, and store the joiner's own
five-valued kind rather than the three-valued scope policy. It touches two files
this lane does not own and it changes an allocation width every existing
allocation site must move with, which is why it is written down rather than done.

## What is not claimed

Nothing was rebuilt. The `javap` verdicts, the preview-gating transcript, the
HotSpot oracle, the two CratonVM baselines, the strict census and the
`ThreadFlock.threadCount` bisection are all measurements of HotSpot 25 and of the
**pre-change** binary. They establish the JDK 25 surface, that the two JDK-21
names are gone, that the deletion is behaviourally inert, and where the real
defect lives. They do not establish that the edited source compiles, that the
added synthetic triples behave, or that the out-of-file patch works.
