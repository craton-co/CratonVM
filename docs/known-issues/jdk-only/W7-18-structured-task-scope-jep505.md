# StructuredTaskScope is a JEP 505 interface now, and the interesting defect was not the stale names

> ## RE-VERIFIED 2026-08-12 (lane A14). The API surface reproduces exactly. **A's gate has since FLIPPED ON and this record still describes it as off.**
>
> Re-measured against Microsoft OpenJDK 25.0.3.9 (this host's `java.home`;
> `javap -version` → 25.0.3), not the Adoptium build §1 used. Nothing was built
> or run on CratonVM.
>
> **1. §1's measured surface reproduces, byte for byte, on a different vendor's
> JDK 25.** `javap` gives exactly the eight members §1 lists, in that order, and
> `javap -v` confirms `PermittedSubclasses: java/util/concurrent/StructuredTaskScopeImpl`
> — so `isSealed=true` and `constructors=0` both hold. The nested-type table is
> right in every row, including the three that must be ABSENT:
> `$ShutdownOnSuccess`, `$ShutdownOnFailure` and `$Config` all answer
> `Error: class not found`. `$Joiner` is a plain `public interface` with no
> `PermittedSubclasses` attribute — **not** sealed, as §1 says, so user code may
> implement it. `$Subtask` and `$Configuration` *are* sealed
> (`…$SubtaskImpl`, `…$ConfigImpl`). `$Joiner`'s five statics and its two
> `default` methods + one abstract `result()` are all present as described.
> `$Subtask$State` declares `UNAVAILABLE, SUCCESS, FAILED` in that order.
>
> This record is **not** stale on the API shape, which was the first thing worth
> checking given how many times JEP 428/437/453/462/499/505 reshaped it.
>
> **2. A has moved since this record was written, and the status block above is
> now wrong about it.** The block says A landed "in this record's *fallback*
> form, **gated**", which reads as off-by-default. It is on by default now:
>
> ```rust
> const VM_REMOVES_THREADS_FROM_CONTAINERS: bool = true;   // shared_secrets_bridge.rs
> ```
>
> and `thread_container_registration_enabled()` falls through to that constant
> unless `CRATONVM_THREAD_CONTAINERS` is `0`/`1`. So on the default
> configuration `jla_start_in_container` now **does** pass the container:
> it reads `args.get(2)`, and when that is a non-null object it calls
> `Thread.start(Ljdk/internal/vm/ThreadContainer;)V`, falling back to
> `start()V` only when the container is null or the flag is `0`. The function's
> own doc comment says so — *"It is no longer dropped by default"* — and cites
> W7-23. **Both registrations survive**, as the block requires.
>
> That means the 15 divergent lines of §5 and the "three CratonVM runs are not
> byte-identical" finding are measurements of a configuration the VM no longer
> ships. They are not refuted — nobody has re-run the probe — but they must not
> be quoted as current. The falsifier in A is now a *regression* check, not a
> pending one.
>
> **All four line references in the status block have drifted** and are wrong as
> written: the function is at `:819` (not 796-816), the gate at `:777` (not
> `:757`), the two registrations at `:1581` and `:1698` (not `:1558`/`:1675`).
> Line numbers in this tree have drifted within a single day; the symbols are
> the durable references and are the ones used above.
>
> **3. B and C are exactly as this record leaves them.** Re-checked by symbol:
> `util_concurrent_ext.rs::register_pd_structured_concurrency` is an empty
> tombstone (`pub(crate) fn …(_r: &mut NativeMethodRegistry) {}`) and is still
> called from `lib.rs`, so the retirement holds and did not orphan its call
> site. `w7_18_jep505_surface_is_not_shadowed_here` is present in
> `jdk25_concurrency.rs`'s test module. C's premise is intact:
> `class_manager.rs` still fabricates `StructuredTaskScope` at
> `instance_fields(8)` and still has a `$Config` row (`instance_fields(3)`), a
> name no JDK ships. **C stays DECLINED for the reasons it already gives** —
> none of the three has changed.
>
> **4. New, found while checking C: `$Subtask`'s fabricated width and its own
> comment disagree.** `class_manager.rs` reads
> `// StructuredTaskScope$Subtask: 4 fields (state=0, result=1, exception=2, callable=3)`
> over `… => instance_fields(5)`. The code is presumably right and the comment
> stale, but this is the exact contract C calls "a *contract* between three
> modules", and a reader auditing slot indices against the comment would count
> four. Comment-only fix **nominated**, not applied — this lane does not own
> `classloading/`. It does not change C's verdict: C is about widening
> `StructuredTaskScope` itself from 8 to 9, not `$Subtask`.
>
> **5. Scope is unchanged and still bounds everything.** All three registrars
> remain reachable only from `register_synthetic_overrides`, so B and C can
> still only move `--synthetic-jdk` mode, which has never been executed. **The
> one thing this record needs is still one run**, and it is now a *different*
> run from the one A needs: A's falsifier wants `probes/StructuredTaskScopeProbe`
> on `--real-jdk`/`--jdk-only` against a current binary to confirm the flipped
> gate closed the 15 lines; B and C want a `--features synthetic-jdk` binary in
> `--synthetic-jdk` **mode**. Neither was available to this lane.

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md).** Of the three
> out-of-file patches: **A** is **APPLIED, but in this record's *fallback* form,
> gated** — commit `4c9482908` made `jla_start_in_container` read `args.get(2)`
> and call `Thread.start(Ljdk/internal/vm/ThreadContainer;)V` behind
> `thread_container_registration_enabled()`
> (`native-builtins/src/shared_secrets_bridge.rs:796-816`). The *preferred*
> form — delete both registrations and the function — was **not** taken; both
> registrations survive at `:1558` and `:1675`. W7-23-thread-container-registration.md
> is why: the preferred form was measured to **hang** when landed without its
> de-registration half.
> **B is PARTIALLY APPLIED as of 2026-08-12** — see
> [B](#b-jdk25_concurrencyrs-still-models-the-jdk-21-shape) for what landed, what
> did not, and the named decision. In one line: the **third** registrar
> (`util_concurrent_ext.rs::register_pd_structured_concurrency`) is retired to a
> tombstone and the shadowing question is now settled by a **test**
> (`w7_18_jep505_surface_is_not_shadowed_here`), while the JDK-21-shaped
> registrations in `jdk25_concurrency.rs` are kept, deliberately, because ~20
> `#[test]`s in a blocking gate pin them and the only mode they can be observed
> in has never been run.
> **C is NOT APPLIED and is now DECLINED with a reason** — see
> [C](#c-two-joiners-cannot-be-served-by-the-8-slot-layout).
> `classloading/src/class_manager.rs` is still `instance_fields(8)` with `$Config`
> (a name no JDK ships) beside it.
>
> * **Residual: OPEN, but the "silently" is gone from B's hazard.**
>   `jdk25_concurrency.rs` does run last and does own every shared triple — the
>   call order was re-derived from the boot path on 2026-08-12 and is
>   `register_phase67_natives` → `register_phase_d_natives` →
>   `register_jdk25_concurrency_natives`, all three inside
>   `register_synthetic_overrides`. But the two triple sets were compared, and
>   **the JEP 505 surface is not shadowed today**; a `#[test]` in
>   `jdk25_concurrency.rs` now fails if that changes.
>   `allSuccessfulOrThrow`/`allUntil` still answer `Void` because slot 8 does not
>   exist; `allUntil`'s `Predicate` is still never consulted; there is still no
>   preview gating in either direction.
> * **Scope, which bounds everything B and C can be worth:** all three
>   `StructuredTaskScope` registrars are reachable **only** from
>   `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]`
>   and called only on `vm_init`'s `use_synthetic_jdk` arm. So none of them is in
>   either shipping binary, `--jdk-only` and `--real-jdk` serve this whole API
>   from real JDK bytecode (§5), and B and C can only move `--synthetic-jdk`
>   **mode** — which, per this directory's §2.6, has never been executed once.
> * **This is the record in the directory most dependent on a run.** See its
>   verification section: `probes/StructuredTaskScopeProbe` on both arms,
>   checking `joinWaits.taskFinishedWhenJoinReturned`,
>   `joinWaits.joinBlockedAtLeast200ms`,
>   `configuration.default.subtaskThreadKind=virtual`, three consecutive
>   byte-identical runs, and watching for `join()` hanging.

**Status: the two dead JDK-21 class names are REMOVED and the JEP 505 surface is
registered, both in `native-builtins/src/phases_late/concurrent.rs`, on the
synthetic-JDK path only. The defect that actually breaks JDK 25 code is in a
different file and is written out below under
[Out-of-file patch — A APPLIED IN FALLBACK FORM; B and C NOT APPLIED](#out-of-file-patch--a-applied-in-fallback-form-b-and-c-not-applied).**
Nothing was
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

## Out-of-file patch — A APPLIED IN FALLBACK FORM; B and C NOT APPLIED

> **The old heading here read "(not applied)" and covered an applied patch.**
> Renamed 2026-08-12; the status block above links to this section.

### A. The defect: `JavaLangAccess.start` drops the ThreadContainer

`native-builtins/src/shared_secrets_bridge.rs`. Two registrations, both in
`register_java_lang_access`, one on `java/lang/System$1` and one on the
`jdk/internal/access/JavaLangAccess` interface, both pointing at
`jla_start_in_container`.

> **DEAD IN THIS FORM — DO NOT APPLY. Reconciled 2026-08-12.** The preferred
> patch stated immediately below was measured to **HANG**: deleting the shadow
> lands the container `add` without the `Thread.exit()` `remove` half, so
> `join()` waits on a counter nothing decrements — the exact failure this
> section's own *Falsifier* warns about last. What landed instead is this
> record's **fallback** form, gated: commit `4c9482908`, `jla_start_in_container`
> reading `args.get(2)` behind `thread_container_registration_enabled()`
> (`native-builtins/src/shared_secrets_bridge.rs:796-816`, gate at `:757`).
> **Both registrations survive at `:1558` and `:1675` and must stay.** See
> W7-23-thread-container-registration.md and W7-55-record-reconciliation.md §2.4.
> Kept below unedited as the statement of why the shadow was wrong.

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

> **PARTIALLY APPLIED 2026-08-12, unrun.** What landed, what did not, and why, is
> the subsection *"B, worked 2026-08-12"* below. The original text is kept
> unedited above it because its inventory is still accurate.

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

#### B, worked 2026-08-12

**First: the registration ORDER, established from the call sequence and not from
brace-scanning, because that is the mechanism the whole hazard rests on.** All
three registrars of `java/util/concurrent/StructuredTaskScope` sit inside
`native-builtins/src/lib.rs`'s `register_synthetic_overrides`, which sets ambient
`NativeKind::Intrinsic` at its top. In call order:

| # | registrar | reached via | ambient kind |
|---|---|---|---|
| 1 | `phases_late/concurrent.rs::register_p67_structured_task_scope` → `…_j25` | `register_phase67_natives` | sets **`Bridge`** locally |
| 2 | `util_concurrent_ext.rs::register_pd_structured_concurrency` | `register_phase_d_natives` | sets **nothing** — inherits `register_synthetic_overrides`' `Intrinsic` |
| 3 | `jdk25_concurrency.rs::register_jdk25_concurrency_natives` | called directly, last of the three | sets **`Bridge`** locally |

`register()` is last-registration-wins, so **#3 wins every triple it shares with
either of the others** — which is what this section says, now checked rather than
asserted. Registrar #2 setting no category of its own is worth noting separately:
it is the one registration window in this family whose kind is decided by a
`set_category` several thousand lines away in a different file.

**What landed (1) — the third registrar is retired.**
`register_pd_structured_concurrency` is now an empty tombstone carrying its own
proof. It was the copy nobody had counted, and it was the dangerous one for a
reason this section did not name: its bodies read and write `$Subtask` **state at
slot 3**, where the registrar that owns the readers (`jdk25_concurrency.rs`:
`SUBTASK_FIELD_STATE = 0`, `RESULT = 1`, `EXCEPTION = 2`, `CALLABLE = 3`) has the
callable **reference**. So deleting a JDK-21-shaped triple from #3 would not have
made a method *absent*; it would have un-shadowed a body that puts an `Int` in a
slot the collector scans as an oop — heap corruption rather than a wrong answer
(docs/architecture/natives-over-real-jdk-classes.md §5).
`t3_impl.rs::register_t31_structured_concurrency` is already a tombstone for
exactly this defect, with the consequence spelled out in place ("registering them
here caused the canonical 8-field layout to be overridden with the earlier
2-field stubs, silently breaking `close()`, `result()`, and `throwIfFailed()`").
This is the copy that pass missed.

**What retiring #2 exposed, and it is a new finding: the two surviving
registrars disagree about `Subtask.State`'s numeric encoding, in the same slot of
the same object.**

| | `Subtask` state slot | UNAVAILABLE | SUCCESS | FAILED |
|---|---|---|---|---|
| `jdk25_concurrency.rs` (`SUBTASK_STATE_*`) — owns `get()`, `state()`, `exception()` | 0 | 0 | **1** | **2** |
| `phases_late/concurrent.rs` (`J25_SUBTASK_STATE_*`) — owns `fork(Runnable)`, `join()Object`, `isCancelled()` | 0 | 0 | **2** | **3** |

Both agree on the slot and disagree on the values, and the split runs straight
through the API: the writers of one JEP 505 path are in the file with one
encoding, the readers are in the file with the other. `SUCCESS` written as `2` is
read back as `FAILED`, so `Subtask.get()` on a *successful* `fork(Runnable)`
subtask raises `IllegalStateException` and `exception()` hands out slot 2. The two
files even carry a comment each explaining that they "agree by value, not by
import" — they do not. This is unreachable in both shipping modes for the same
structural reason as everything else here, which is exactly why nothing has ever
noticed it, and it is the strongest single argument that B's remainder needs the
run rather than more reading.

The retirement is **provably inert**, which is why it was safe to take blind:
#2's `StructuredTaskScope` and `$Subtask` triples are a strict subset of #3's, so
every one of them was already overwritten. The only two registrations it ever
won are the covariant-return `join()`s —
`$ShutdownOnFailure.join()L…$ShutdownOnFailure;` and
`$ShutdownOnSuccess.join()L…$ShutdownOnSuccess;`, which #3 spelled with the base
`L…StructuredTaskScope;` return and so did not collide with — and both are on
classes JEP 505 deleted. That is the same covariant-return pattern §3 found when
comparing this file against #1.

**What landed (2) — the shadowing question is now a test.**
`jdk25_concurrency.rs` grows `w7_18_jep505_surface_is_not_shadowed_here`, which
asserts that none of the nine JEP 505 triples owned by #1 is registered by #3.
It is a **ratchet, not coverage**: it does not fail on the old behaviour, because
the old behaviour does not have the shadow — the two sets were compared and the
JEP 505 surface is clean today. What it does is convert "runs last, so it *can*
silently re-impose the JDK-21 shape" from a standing hazard into a compile-and-
test-time refusal. It is a `cargo test`, not a scheduled fixture, and it is
labelled as such at the site.

**What did NOT land, named as a decision.** The ~24 `$ShutdownOnSuccess` /
`$ShutdownOnFailure` triples, `$Config`, `Joiner.policy()I`, and the JDK-21-only
`StructuredTaskScope` methods (`<init>` ×3, `joinUntil`, `shutdown`,
`isShutdown`, `join()LStructuredTaskScope;`) are all still registered. An
in-file `JDK-ONLY-NOTE (W7-18)` at the `ShutdownOnFailure` block records the
measured verdict so the next reader does not re-derive it. The decision rests on
two conditions of which only one holds:

* *It cannot move either shipping mode.* **True** — and therefore the deletion is
  also worth nothing there. §5 measured `compatibility_classes: 0`,
  `synthetic_stub_invocations: 0` and zero StructuredTaskScope violations in 1,454
  under `--jdk-only`; real JDK bytecode serves the whole API.
* *The deletion is checkable.* **False.** Roughly twenty `#[test]`s in
  `jdk25_concurrency.rs`'s own module pin these registrations by triple — the
  nine `test_register_sof_*` / `test_register_sos_*`,
  `test_all_shutdown_on_failure_methods_registered` and its `_success_` twin,
  `s52_joiner_policy_registered`, `s52_total_registration_count`, and the
  fork/join/close/shutdown lists.
  Deleting the registrations means rewriting a **blocking gate's** assertions to
  match an expectation that no run has ever produced, in the one mode
  (`--synthetic-jdk`) that this directory's §2.6 records as never having been
  executed. That is the shape
  docs/known-issues/jdk-only/W6-5-vacuous-tests.md warns about from the other
  direction: a test that freezes VM output locks the divergence in, and editing it
  blind moves the freeze rather than removing it.

**What the next person needs, and it is one run, not a redesign.** Build
`--features synthetic-jdk` and run `probes/StructuredTaskScopeProbe` in
`--synthetic-jdk` **mode** (W7-50 built the feature binary but ran it under
`--jdk-only`, where none of this registers). With that transcript in hand the
deletion becomes a measured change and the twenty tests can be rewritten against
an observed answer instead of a guessed one. Until then, deleting is the more
expensive of the two mistakes available.

**Untouched, and still open as written:** `SCOPE_OWNERS` / `SCOPE_JOINERS` are
still keyed on `ObjectRef::as_ptr() as usize`. `SCOPE_FORKS`, immediately above
them in the same file, carries the correct remedy in its own doc comment (key by
`ctx.identity_hash_code(scope)`, re-read values through the
`(identity_key, ObjectRef)` var-handle-root pattern) and has not taken it either,
so all three want one pass rather than three. That pass needs a `ctx` at every
call site, which several of them do not have today.

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

> **DECLINED 2026-08-12, with the reason, rather than left open-ended.** Three
> things have to be true for this to be worth taking blind, and none of them is:
>
> 1. **It is not a one-file change and the widening is the dangerous half.**
>    `classloading/src/class_manager.rs`'s `instance_fields(8)` is a *contract*
>    between three modules: `jdk25_concurrency.rs` allocates against it,
>    `phases_late/concurrent.rs` reads slots 0–7 by index against it, and
>    `class_manager.rs` fabricates it. A ninth index written against an
>    allocation that is still eight wide is heap corruption, not a wrong answer
>    (docs/architecture/natives-over-real-jdk-classes.md §5), and the three edits
>    have to land in one commit or the intermediate state is the corruption. Two
>    of the three files belong to other lanes.
> 2. **The only mode it can be observed in has never been run.** Everything here
>    is `--synthetic-jdk`-mode-only, structurally (see the status block). So the
>    change would be written, landed, and validated by nothing — which is the
>    same position W6-12's residual is in, and this directory has stopped
>    treating that as progress.
> 3. **The record's own reasoning says the safe direction is where we already
>    are.** `allUntil` currently mints an `awaitAll` joiner: a predicate that
>    never fires, i.e. it **over-waits** rather than under-waits, which is the
>    safe error when every subtask has already run under this file's synchronous
>    fork. `join()` answering `Void` for the two stream joiners is likewise a
>    refusal rather than a fabricated empty `Stream` — and this section already
>    argues, correctly, that the fabricated stream is the worse outcome. Taking
>    the patch blind risks converting a documented refusal into an unmeasured
>    wrong answer.
>
> **What would unblock it:** the same single run B needs — a
> `--features synthetic-jdk` binary in `--synthetic-jdk` **mode** with
> `probes/StructuredTaskScopeProbe`. With that transcript, C becomes a
> three-file change with an oracle. Without it, C is a three-file change with a
> hypothesis.

## What is not claimed

Nothing was rebuilt. The `javap` verdicts, the preview-gating transcript, the
HotSpot oracle, the two CratonVM baselines, the strict census and the
`ThreadFlock.threadCount` bisection are all measurements of HotSpot 25 and of the
**pre-change** binary. They establish the JDK 25 surface, that the two JDK-21
names are gone, that the deletion is behaviourally inert, and where the real
defect lives. They do not establish that the edited source compiles, that the
added synthetic triples behave, or that the out-of-file patch works.

---

## B and C adjudicated in `--synthetic-jdk` — 2026-08-12 (lane A31)

This record says, five times, that B and C "can still only move `--synthetic-jdk`
mode, which has never been executed", and that it is "the record in the directory
most dependent on a run". The run was done: `--features synthetic-jdk` binary
built from clean HEAD, `probes/StructuredTaskScopeProbe` compiled
`--release 25 --enable-preview`, launched
`cratonvm --synthetic-jdk --enable-preview -cp … StructuredTaskScopeProbe`.

**Verdict: B and C are UNREACHABLE even in `--synthetic-jdk`. Not "wrong" — not
runnable.** Every probe row is a setup failure, and they are all the same two:

```
--synthetic-jdk:
  openDefault.threw            = java.lang.NullPointerException: Cannot invoke
      "java.lang.reflect.Method.invoke(Object, Object[])" because
      "StructuredTaskScopeProbe.M_OPEN0" is null
  joinWaits.threw              = (same)
  forkRunnable.threw           = (same)
  ownerThread.threw            = (same)
  subtaskCarrier.threw         = (same)
  state.*.setup                = (same, ×5)
  joinerAwaitAll.threw         = java.lang.NoSuchMethodException: awaitAll
  awaitAllSuccessful.happyPath.threw = java.lang.NoSuchMethodException: awaitAllSuccessfulOrThrow
  awaitAllSuccessful.failPath.threw  = java.lang.NoSuchMethodException: awaitAllSuccessfulOrThrow
  allSuccessful.threw          = java.lang.NoSuchMethodException: allSuccessfulOrThrow
  anySuccessful.threw          = java.lang.NoSuchMethodException: anySuccessfulResultOrThrow
  anySuccessful.allFail.threw  = java.lang.NoSuchMethodException: anySuccessfulResultOrThrow
  allUntil.threw               = java.lang.NoSuchMethodException: allUntil
  configuration.threw          = java.lang.NoSuchMethodException: awaitAll
  timeout.threw                = java.lang.NoSuchMethodException: awaitAll
```

`M_OPEN0` is `StructuredTaskScope.class.getMethod("open")`
(`StructuredTaskScopeProbe.java:253`). It resolves to null, so **no scope object
can be constructed at all**, so:

* **C is moot in practice.** C is about widening the fabricated
  `StructuredTaskScope` from `instance_fields(8)` to 9 so two `Joiner`s can be
  served. No instance is ever allocated, so no slot is ever read or written. C's
  DECLINED verdict stands, and can now be stated more strongly: it is declined
  *and* unobservable.
* **B's kept JDK-21-shaped registrations in `jdk25_concurrency.rs` are
  unreachable from bytecode in all three configurations.** The record's reason
  for keeping them — "~20 `#[test]`s in a blocking gate pin them and the only
  mode they can be observed in has never been run" — now reads differently: that
  mode has been run, and it does not reach them either. The tests pin code no
  Java caller can enter. That is not an argument to delete them blind, but it
  removes the "it might be load-bearing in synthetic mode" half of the argument
  for keeping them.

HotSpot 25 (`--enable-preview`) is the negative control and answers all 26 rows,
e.g. `state.joinTwice=java.lang.IllegalStateException:Already joined or scope is
closed`, `subtask.state.declaringClass=java.util.concurrent.StructuredTaskScope$Subtask$State`,
`ownerThread.forkFromOther=java.lang.WrongThreadException:Current thread not owner`.

### One thing the run found that this record predicted structurally and never measured

`Class.forName` **succeeds for every name the probe asks for** in
`--synthetic-jdk`, including names JDK 25 does not ship:

```
--synthetic-jdk:                                    HotSpot 25:
  forName.jdk.incubator.concurrent.StructuredTaskScope
      = jdk.incubator.concurrent.StructuredTaskScope     ClassNotFoundException
  forName.jdk.incubator.concurrent.StructuredTaskScope$Subtask
      = …$Subtask                                        ClassNotFoundException
  forName.jdk.incubator.concurrent.StructuredTaskScope$ShutdownOnSuccess
      = …$ShutdownOnSuccess                              ClassNotFoundException
  forName.jdk.incubator.concurrent.StructuredTaskScope$ShutdownOnFailure
      = …$ShutdownOnFailure                              ClassNotFoundException
  forName.java.util.concurrent.StructuredTaskScope$Config
      = …$Config                                         (a name no JDK ships)
```

C's complaint — *"`class_manager.rs` still has a `$Config` row, a name no JDK
ships"* — is confirmed from the outside: the name resolves. And the JDK-21
*incubator* package, deleted in JDK 25, resolves too, along with its three
nested types. This is the `Ok≠use` shape at full strength: **every metadata query
answers yes and every use fails.** A census or a `forName`-based feature probe
run in this mode will report the JEP 505 surface as present and complete.

**Nothing here is a work item for this record.** It is the run the record asked
for, with the answer that the run cannot discriminate B or C. What it does
settle is that this record should stop describing itself as blocked on a
`--synthetic-jdk` run — it is blocked on `StructuredTaskScope.open()` being
registered, which is a prior and much larger question.
