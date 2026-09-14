# The common-pool factory was bound by name, and the name was from JDK 21

**Status: FIXED 2026-08-11 on the `--jdk-only` path. `Compatible` deliberately
NOT changed — the correct fix for that half is written out below under
[Out-of-file decision required](#out-of-file-decision-required) and is a human's
call.** Not rebuilt in this lane: no claim is made that the change compiles or
that the vector passes. Every measurement below was taken with the pre-change
binary at `C:/craton/CratonVM/target/release/cratonvm.exe` and with Adoptium
JDK 25.0.3.9, and each is reproducible.

> ## Re-verified 2026-08-12 (record-triage lane, doc-only — nothing built or run)
>
> A source read of today's tree, stated as such. Line numbers below are today's.
>
> * **The strict fix is present and is exactly what this record describes.**
>   `common_factory_from_image` is at
>   `native-builtins/src/phases_late/concurrent.rs:8909`, and
>   `alloc_common_factory` (`:8933-8987`) calls it only from the `Err(_)` arm,
>   **after** `try_alloc_concurrent_synthetic`. The ordering this record calls
>   the contract is intact, comment and all.
> * **The `Compatible` half is still NOT taken.**
>   `resolve_common_factory_internal_name` (`:8864-8873`) still returns the
>   JDK-21 name and its doc comment still hands the decision back. The
>   out-of-file patch below is unapplied; it remains a human's call.
> * **The sweep table's line numbers are stale.** The file has moved ~690 lines:
>   the fabrication-target rows are now `:8871` (the fallback string) and
>   `:9103` (`t19_k3_safe_factory_class_name_accepts_quarkus`).
> * **The two `StructuredTaskScope$ShutdownOn*` rows are no longer true as
>   written, and in the good direction — treat them as CLOSED.** No
>   `java/util/concurrent/StructuredTaskScope$ShutdownOn{Success,Failure}`
>   registration exists in this file any more; the JEP 505 lane deleted the ~16
>   of them and left the tombstone at `:5429-5437`. What survives is the
>   `jdk/incubator/concurrent/` spelling (`:4574`, `:4629`) inside
>   `register_p67_structured_task_scope` — dead for a *different* reason (JDK 25
>   ships no `jdk.incubator.concurrent`), already saying so at `:4431-4448`, and
>   counted by `W7-5-registrars-that-never-shipped.md`. Do not re-file it here.
> * **`SynchronousQueue$Itr` is unchanged and still dead** —
>   `try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/SynchronousQueue$Itr", 2)`
>   at `:1864`. Still the never-shipped-registrar census owner's deletion, not
>   this record's.
> * **The fixture handles the common-pool parallelism divergence correctly and
>   wants no change.** `regression-suite/src/RJdkForkJoin.java:212-260` pins
>   `getCommonPoolParallelism()` to `max(1, procs-1)` **or** the documented
>   inline-proxy clamp of 1, asserts the static and instance readouts agree, and
>   prints only VM-independent facts on the `CK` line (`commonParAccepted=true
>   singleCpuHost=…`). CratonVM clamps the common pool to 1 for determinism where
>   HotSpot answers `max(1, procs-1)`; printing the number would fail the
>   harness's cross-VM `CK` diff on every host with 3+ CPUs. That divergence is
>   legitimate and is not this record's defect.
> * **Scheduling: half.** `RJdkForkJoin` is in `JDKONLY_CLASSES`
>   (`regression-suite/run.sh:119`), so falsifier bullet 1 — the vector getting
>   past `parallelStreams():209` — is scheduled and will run. **Bullets 2 and 3
>   are not scheduled.** Nothing in the fixture reads `getFactory()`, so neither
>   the strict-side answer nor the "`Compatible` must not move" guard has an
>   executable form; the evidence for both is this record's hand-run probes. A
>   single uniform assertion is impossible while the two modes are *specified*
>   here to answer differently — which is itself an argument for taking the
>   Compatible half rather than leaving it. The interim shape, if someone wants
>   cover before that decision, is the accepted-set form the parallelism check in
>   the same file already uses: assert the class name is one of the two known
>   answers and that the identity relation agrees with whichever name came back,
>   so a *third* answer fails. It would not have failed the pre-fix binary (that
>   one threw earlier), so it is cover against future rot, not a RED.


> **VERIFIED AGAINST A BINARY 2026-09-04. The Falsifier was run as written, all
> three rows, and the change SHOULD STAY.** The status block says *"Not rebuilt
> in this lane: no claim is made that the change compiles or that the vector
> passes."* Both claims now have a measurement.
>
> **Row 1 — the vector gets past `parallelStreams():209`.**
>
> ```text
> HotSpot 25            PASS RJdkForkJoin (46 checks)
> CratonVM --jdk-only   PASS RJdkForkJoin (46 checks)
> CratonVM compatible   PASS RJdkForkJoin (46 checks)
> ```
>
> The `NoClassDefFoundError` naming
> `…$DefaultCommonPoolForkJoinWorkerThreadFactory` is gone.
>
> **Rows 2 and 3 — `probes/FjpCommonFactoryProbe.java`**, written to print
> exactly the two expressions the Falsifier names:
>
> ```text
>                       getFactory().getClass().getName()                    == defaultFJWTF
> HotSpot 25            ForkJoinPool$DefaultForkJoinWorkerThreadFactory        true
> CratonVM --jdk-only   ForkJoinPool$DefaultForkJoinWorkerThreadFactory        true
> CratonVM --real-jdk   ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory   false
> CratonVM compatible   ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory   false
> ```
>
> **This record staked the fix on the third row**: *"If Compatible moves, this
> lane's ordering argument is wrong and the change should be reverted, not
> patched"* — the point of putting the recovery after
> `try_alloc_concurrent_synthetic` being that Compatible never reaches it.
> **Compatible did not move.** It reports the common-pool factory and `false`,
> identical to `--real-jdk`, so the ordering argument holds. The factory is also
> not null in any arm, which rules out the record's other stated failure mode
> ("the factory comes back null ... the resolution context is the problem").
>
> **The recovery path is visible in the transcript, not merely inferred.** The
> strict arm logs the refusal first and then answers correctly:
>
> ```text
> WARN refusing to fabricate a compatibility stand-in …
>   class="java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory"
>   requested_by="native-builtins/src/phases_late/concurrent.rs:8833"
> ```
>
> That is this record's own file and line reaching the recovery, so the arm
> under test is the arm described.
>
> **What this does NOT verify.** The `DelayedWorkQueue` row is noted in the
> record as *"right today ... a package-private JDK-internal name with no test
> pinning it"*; nothing here pins it either, and it was not re-run. The
> Compatible half of the fix that this record deliberately did NOT make — the
> "Out-of-file decision required" section, a human's call — is untouched and
> remains open; the `false` above is that decision still pending, not a defect
> this note clears. 46 checks against the record's earlier counts is shared
> vector growth, not a result.
## The failure

```
NoClassDefFoundError: java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory
  at RJdkForkJoin.parallelStreams(RJdkForkJoin.java:209)
  at RJdkForkJoin.main(RJdkForkJoin.java:305)
```

Nothing in `parallelStreams()` is about thread factories. It calls
`ForkJoinPool.commonPool()`, `commonPool()` populates a `factory` field, and the
population names a class.

Pre-existing, not a regression: the control measurement against `7d4d545e0` is
in docs/known-issues/jdk-only/W7-11-strict-baseline-remeasured.md, which records
this vector failing byte-identically before this wave's branches merged.

The strict census names the site outright — `--jdk-only-report`, wave-1
`compatibility-class-requested` record:

```json
{"kind":"compatibility-class-requested",
 "class":"java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory",
 "requester":"native-builtins\\src\\phases_late\\concurrent.rs:8212",
 "reason":"VM-requested stand-in: ensure_synthetic_class called with no class file on any classpath entry"}
```

## The `javap` verdict on JDK 25

`java.util.concurrent.ForkJoinPool` declares **five** nested classes on JDK 25,
and the one the code asks for is not among them:

| nested class | JDK 25 (`javap -p`) |
|---|---|
| `ForkJoinPool$ForkJoinWorkerThreadFactory` | present — `public interface` |
| `ForkJoinPool$DefaultForkJoinWorkerThreadFactory` | present — `final class`, implements the above |
| `ForkJoinPool$WorkQueue` | present |
| `ForkJoinPool$ManagedBlocker` | present |
| `ForkJoinPool$TimeoutAction` | present |
| **`ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory`** | **`Error: class not found`** |

That list is `ForkJoinPool.class.getDeclaredClasses()` run on HotSpot 25, and
the last row is `javap -p` on the requested name. The missing class is a JDK-21
artifact: back when a security manager existed, the *common* pool needed a
factory that cleared permissions on its workers. That reason went away and so
did the class.

## What HotSpot 25 actually answers

```
commonPool().getFactory().getClass()      = java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory
defaultForkJoinWorkerThreadFactory.class  = java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory
same instance                             = true
```

**`same instance = true` is the finding that changes the fix.** The common
pool's factory is not merely *an instance of the same class* as the public
static field `ForkJoinPool.defaultForkJoinWorkerThreadFactory` — it *is* that
object.

## Why the fix is not "update the string"

Swapping `…$DefaultCommonPool…` for `…$Default…` would be correct today and
would rot at the next release exactly as this one did, silently, because nothing
in the tree tests the answer. That is the shape five separate defects in this
campaign reduced to a single rule: **never bind by name**.

The static field is a different risk class, for two separable reasons:

* `ForkJoinPool.defaultForkJoinWorkerThreadFactory` is `public static final`
  **specified API**, present since Java 7. A name the specification publishes is
  not a name the implementation happens to use this year.
* Because HotSpot hands back that very instance, reading the field reproduces
  reference identity as well as class identity. Sharing the singleton is
  therefore *more* faithful than allocating a fresh object, not a shortcut.

So the fix asks the image, via the new `common_factory_from_image` in
`native-builtins/src/phases_late/concurrent.rs`:

```rust
let cid = ctx.ensure_class_initialized("java/util/concurrent/ForkJoinPool").ok()?;
let idx = ctx.static_field_index_by_name(cid, "defaultForkJoinWorkerThreadFactory")?;
match ctx.get_static_field(cid, idx) { Value::Object(Some(f)) => Some(f), _ => None }
```

**Is resolving from the image reachable? Yes, and it is measured, not assumed.**
A probe run on the pre-change binary under `--jdk-only`:

```
STATIC defaultForkJoinWorkerThreadFactory = java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory
commonPool path threw: java.lang.NoClassDefFoundError: java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory
```

The right object was sitting in a readable static field in the same strict-mode
process that threw. `--jdk-only` refuses fabrication, so the class behind that
name can only be the real one out of the image.

`ensure_class_initialized` rather than `class_id_by_name`, because the field is
written by `ForkJoinPool.<clinit>`; an uninitialized class reads null and would
send us down the fallback for no reason. The helper returns `Option`, never an
error, so a JDK that stops publishing the field degrades to today's behaviour
instead of failing.

## What changed, and the ordering that keeps `Compatible` still

Only the `Err(_)` arm of `alloc_common_factory`, and only *after* the existing
call:

```rust
match try_alloc_concurrent_synthetic(ctx, &target, 1) {
    Ok(factory) => Ok(factory),
    Err(refusal) => match common_factory_from_image(ctx) {
        Some(factory) => Ok(factory),
        None => Err(refusal),
    },
}
```

The order is the contract. Recovery runs *after* fabrication, so `Compatible` is
untouched by construction rather than by argument: where fabrication is
available it still succeeds, still succeeds first, and still returns the object
built by the same call — including the operator's own
`java.util.concurrent.ForkJoinPool.common.threadFactory` class, which is what
Keycloak/Quarkus's `getFactory().getClass().getName().equals(property)` check
reads.

This is deliberately **not** the probe-first variant recorded in W6-12. That
variant calls `try_ensure_synthetic_class(&target, 1)` before
`try_alloc_concurrent_synthetic`, which in `Compatible` mints the class one call
earlier; the subsequent `ensure_class_initialized` inside
`try_alloc_concurrent_synthetic` then *finds* it and allocates through
`try_alloc_object_gc_safe` instead of the bare `alloc_object` the refusal path
uses. Same class, same slot count, different allocator entry point. Small, and
still a `Compatible` change this lane may not make.

The price of ordering it this way is paid only where the refusal happens: the
discarded `Err` is a `NoClassDefFoundError` that was allocated and is now
garbage. `refusal_to_java_failure` returns `MethodCallFailed::ExceptionThrown`
as a *value* with no pending VM state, so dropping it is clean.
`getFactory()` caches into `factory`, so this costs about one throwable per
process — the right trade for a mode that must not move.

Falling back to the default factory when the requested one cannot be produced is
also what the real `ForkJoinPool.<clinit>` does: it catches the property-named
factory's failure and keeps `defaultForkJoinWorkerThreadFactory`. This is
specified behaviour, not a strict-mode concession.

## Out-of-file decision required

**The quiet half is the more interesting one, and it is not being applied.**

`Compatible` mode is measurably wrong here, and nothing tests it. Measured on
the pre-change binary, `--real-jdk`, JDK 25, property unset:

| | `getFactory().getClass().getName()` | `== defaultForkJoinWorkerThreadFactory` |
|---|---|---|
| HotSpot 25 | `…ForkJoinPool$DefaultForkJoinWorkerThreadFactory` | **true** |
| `cratonvm --real-jdk` | `…ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory` | **false** |

So `--real-jdk` hands back an instance of a **fabricated class the image does
not declare**, and gets the identity relation wrong as well. It does this
silently, which is why it survived: strict mode throws and gets a bug record,
Compatible answers wrongly and gets nothing.

### The exact patch

Move the image lookup *ahead* of fabrication, but **only when the name is the
built-in default** — never when the operator supplied it:

```rust
/// The built-in fallback returned by `resolve_common_factory_internal_name`.
/// Named so the recovery below can tell "nobody asked for anything specific"
/// from "the operator asked for this and it did not load".
const DEFAULT_COMMON_FACTORY_INTERNAL_NAME: &str =
    "java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory";

// ... in `alloc_common_factory`, replacing the `Err(_)` arm's body:
        Err(_) => {
            // W7-14 Compatible half. Ask the image BEFORE fabricating, in every
            // mode, so `--real-jdk` stops answering with a class JDK 25 does
            // not declare. Gated on the default name: when the operator named a
            // factory via `…common.threadFactory` and it failed to load, the
            // fabricated stand-in under *their* name is what
            // `getFactory().getClass().getName().equals(property)` reads, and
            // substituting the JDK default there would break that check.
            if target == DEFAULT_COMMON_FACTORY_INTERNAL_NAME {
                if let Some(factory) = common_factory_from_image(ctx) {
                    return Ok(factory);
                }
            }
            try_alloc_concurrent_synthetic(ctx, &target, 1)
        }
```

With this applied, the strict-side `match` shown earlier becomes redundant and
should be collapsed back to the single `try_alloc_concurrent_synthetic` call —
both modes then take the same path, which is the point.

Note that the gate is load-bearing and W6-12's recorded fidelity variant does
not have it: that variant's candidate list falls through an unloadable
operator-supplied name to the JDK default, which is the Keycloak/Quarkus
breakage described above. It is only invisible because the Quarkus factory is
genuinely on the classpath in the vector that exercises it.

### What changes for a caller

On JDK 25 with the property unset, `--real-jdk`:

* `ForkJoinPool.commonPool().getFactory().getClass().getName()` changes from
  `java.util.concurrent.ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory`
  to `java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory`.
* `getFactory() == ForkJoinPool.defaultForkJoinWorkerThreadFactory` changes from
  `false` to `true`.
* Both new answers match HotSpot 25 exactly, as measured above.

Anything string-matching the old invented name would break. Nothing in-tree
does: a `grep` for `DefaultCommonPool` across `*.rs`, `*.java` and `*.md` finds
only this record, W6-12, W7-11, and the two sites in
`native-builtins/src/phases_late/concurrent.rs` themselves. The one test that
mentions the name, `t19_k3_safe_factory_class_name_accepts_quarkus`, only
asserts the allowlist validator accepts a `$`; its comment claimed the string
was "the JDK default", which was the stale reading that kept the name looking
load-bearing, and that comment has been corrected with the JDK 25 spelling
asserted alongside it.

**Why it is nonetheless the right answer.** Compatible mode's contract is
fidelity to a real JVM. Here it is not merely diverging from HotSpot, it is
returning an object whose class does not exist in the image it claims to be
running — a fabrication with no referent, presented to user code as a JDK type.
The §5/§10 rule exists to stop lanes changing observable Compatible behaviour as
a side effect of fixing strict mode; it is not a reason to keep an answer that
is wrong against the very JDK the mode is defined by. That is exactly why this
is being handed back rather than taken.

## Sweep: every hard-coded `java.util.concurrent` nested name in this file

24 occurrences, 10 distinct names. `javap -p` verdict on Adoptium JDK 25.0.3.9
for each:

| internal name | line(s) | JDK 25 declares it | how it is used | verdict |
|---|---|---|---|---|
| `ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory` | 8183, 8201 | **NO** | fabrication target | **the defect — fixed strict-side above** |
| `StructuredTaskScope$ShutdownOnSuccess` | 5133, 5138 | **NO** | native registration | **rotted, same JDK-21 era — not fixed, see below** |
| `StructuredTaskScope$ShutdownOnFailure` | 5158, 5163 | **NO** | native registration | **rotted, same JDK-21 era — not fixed, see below** |
| `SynchronousQueue$Itr` | 1857 | **NO — and no JDK ever did** | fabrication target | **CratonVM invention in a JDK package; measured unreachable — see below** |
| `ScheduledThreadPoolExecutor$DelayedWorkQueue` | 2943, 2947 | yes (package-private class) | `new_object` + `invoke_special` | correct today; version-fragile, recorded |
| `Flow$Subscription` | 2468, 2609, 2621 | yes (`public interface`) | fabrication target + descriptor | name correct; *instantiating an interface* at 2609 is a separate question |
| `Flow$Subscriber` | 2588 | yes (`public interface`) | descriptor only | fine |
| `ForkJoinPool$ForkJoinWorkerThreadFactory` | 8101, 8126, 8415 | yes (`public interface`) | descriptor only | fine — a descriptor *must* match the image's spelling |
| `StructuredTaskScope$Subtask` | 4919, 5071, 5075, 5139, 5164 | yes (`public interface`) | registration + descriptors | fine |
| `StructuredTaskScope$Subtask$State` | 5089, 5103 | yes (`public final class`, enum) | fabrication target + descriptor | fine |

### The two `StructuredTaskScope` names are the same defect, twice more

JDK 25 redesigned `StructuredTaskScope` (JEP 505): it is now an **interface**,
opened with `open(Joiner)` and configured with `Configuration`. The
`ShutdownOnSuccess` / `ShutdownOnFailure` nested policy classes are gone,
replaced by `Joiner` factories. So two more names in this file are JDK-21-era
spellings of a thing JDK 25 does not declare.

They are **not fixed here, and the reason is not scope-shyness**: unlike the
factory, this is not a name that has a correct spelling to resolve to. The
replacement is a different API shape, and re-registering these bodies against
`Joiner` is a feature, not a rename. Recorded so the next reader does not have
to re-derive it.

Their failure mode is also quieter than the factory's: these are *registrations*
on a class name, not fabrication requests. A registration on a class the image
never declares simply never binds — the shape recorded as an inert registration
looking exactly like a missing feature, and censused in
docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md. They do not
appear in the `--jdk-only-report` census for the `ForkJoinPool` vector, because
that vector never reaches them; that is consistent with inertness but does not
prove it, and no probe was run for them.

### `SynchronousQueue$Itr` is a different animal, and it is dead

No JDK declares it. Real `SynchronousQueue.iterator()` returns
`Collections.emptyIterator()`, so this is a CratonVM-invented helper name minted
inside the `java.util.concurrent` package. Measured on the pre-change binary:

```
HOTSPOT:              SynchronousQueue.iterator() = java.util.Collections$EmptyIterator hasNext=false
cratonvm --jdk-only:  SynchronousQueue.iterator() = java.util.Collections$EmptyIterator hasNext=false
```

Strict mode agrees with HotSpot exactly, which means the native at line 1857 is
**not reached** in real-JDK mode — the real bytecode runs and the stub is dead
code holding a fabrication request that would be refused if anything ever got to
it. Deleting it is the right answer and belongs to whoever owns the
never-shipped-registrar census, not to a name-binding fix.

The same probe confirms the `DelayedWorkQueue` row: under `--jdk-only`,
`new ScheduledThreadPoolExecutor(1).getQueue().getClass().getName()` reports
`java.util.concurrent.ScheduledThreadPoolExecutor$DelayedWorkQueue`, matching
HotSpot. That name is right today. It is still a package-private JDK-internal
name with no test pinning it, i.e. the same rot risk at one remove — worth
recording precisely because it happens to be correct.

## Falsifier

Under `--jdk-only` on JDK 25, after a rebuild:

* `RJdkForkJoin` should get past `parallelStreams():209` — the
  `NoClassDefFoundError` naming `…$DefaultCommonPoolForkJoinWorkerThreadFactory`
  should be gone. Other failures in that vector would be separate findings; this
  fix only claims the first `commonPool()` stops throwing.
* `ForkJoinPool.commonPool().getFactory().getClass().getName()` should print
  `java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory`, and
  `getFactory() == ForkJoinPool.defaultForkJoinWorkerThreadFactory` should be
  `true` — both matching HotSpot 25.
* Under `--real-jdk` the same two expressions must still report
  `…$DefaultCommonPoolForkJoinWorkerThreadFactory` and `false`. **If Compatible
  moves, this lane's ordering argument is wrong and the change should be
  reverted, not patched** — the whole point of putting the recovery after
  `try_alloc_concurrent_synthetic` is that Compatible never reaches it.

If instead strict throws `NoClassDefFoundError` on
`…$DefaultForkJoinWorkerThreadFactory`, or the factory comes back null, then the
static field was not readable at that point in initialization and the resolution
context is the problem, not the name — note that the probe above read it
successfully from Java bytecode, which forces `<clinit>` first.

## What is not claimed

Nothing was rebuilt in this lane. The measurements are all of the *pre-change*
binary and of HotSpot 25; they establish the diagnosis, the `javap` verdicts,
the reachability of the static field, and the Compatible-mode baseline. They do
not establish that the edited source compiles or that the vector passes.
