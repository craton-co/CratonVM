# StampedLock was already fixed; Phaser was the live split-brain

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Primary claim: REFUTED, and nothing about it is outstanding.** `StampedLock`
  needs no change — the losing `native-collections` registrar was disabled on
  2026-07-28 and its call site is a `let _ =`. The surviving implementation is
  `native-builtins/src/util_concurrent_ext.rs::register_stamped_lock_natives`.
  This half of the record is closed and should not be reopened by a future
  duplicate-registration census: the census counts `r.register(...)` call sites
  and **does not ask whether the function is reachable**, which is exactly how
  the false brief was produced.
* **Sibling claim: CONFIRMED and fixed in source.** `Phaser` had the described
  defect plus a second one; the fix is the dropped call site
  (`let _ = register_phaser_natives;`) in
  `native-collections/src/lib.rs::register_collections_natives`.
* **Residual: CLOSED — the two miswritten `native-collections` comments** (CHM
  `KeySetView`, `LinkedList` `first`/`size`) were corrected in place,
  `native-collections/src/lib.rs:2597` and `:30860-30870`, and written up in
  W2-1-strict-refuses-the-synthetic-stream-stack.md.
* **Residual: CLOSED ELSEWHERE, and BOTH of this record's prescriptions were
  rejected.** §3's `ForkJoinPool.commonPool()` defect — a factory class named
  `…$DefaultCommonPoolForkJoinWorkerThreadFactory`, a JDK-21 name JDK 25 does
  not declare — is fixed by commit `46bb0ad2e` *fix(jdk-only): resolve the
  common-pool factory from the image, not a JDK-21 name*, filed as
  W7-14-fjp-common-factory-bound-by-name.md.
  `common_factory_from_image`
  (`native-builtins/src/phases_late/concurrent.rs:8524-8538`) reads the **public
  static field** `ForkJoinPool.defaultForkJoinWorkerThreadFactory` instead of
  naming any nested class. Neither recorded variant was taken, deliberately:
  the *conservative* variant's ordering is refused in a comment at
  `concurrent.rs:8566-8577` — *"ORDER IS THE CONTRACT HERE … probing the policy
  first (the variant recorded in W6-12) would mint the class one call earlier"* —
  and the *fidelity* variant's three-name candidate list was not taken because
  it would have changed `--real-jdk`, which this record itself said was not this
  lane's to take. **Do not apply the patch below.**
* **Residual: MEASURED 2026-08-12 (lane A31) — the stated claim is FALSIFIED
  and the family is wrong at six other members.** See §"The `Collections`
  fidelity residual, adjudicated" at the foot of this record. The residual is
  **not retired**: its specific asymmetry does not exist, but the surface it
  pointed at is worse than it described.
* **Residual: SUPERSEDED — was "STILL OPEN, and unmeasurable on a shipping
  binary."** The `Collections` fidelity residual is structurally confined to
  `synthetic-jdk` — both shipping modes drop the contested factories and run real
  `java.util.Collections` bytecode, and all 37 rows of
  `probes/JdkOnlyCollectionViewProbe` are byte-identical across HotSpot,
  `--real-jdk` and `--jdk-only`. It was open only in the sense that **no
  `synthetic-jdk` binary had ever been built to measure it**. One now has been.

  > **AMENDED 2026-08-12 (W7-78-inherited-residual-closeout.md) — the
  > confinement is now proved by construction, and the "never built" half of
  > this bullet is stale.**
  >
  > **The confinement is stronger than this record argues.** §1 below reasons
  > from registration ORDER — that `phases_early`'s identity bindings run after
  > the essential registrars and so win under `--synthetic-jdk`. Order is not
  > what confines them. `native-builtins`'
  > `phases_early::register_collections_extras_natives`
  > (`native-builtins/src/phases_early.rs:100`, ambient kind **`Intrinsic`** —
  > note that `Intrinsic` is NOT dropped under `JdkOnly`, so order would not
  > have saved this) is reached only from `lib::register_synthetic_overrides`
  > (`native-builtins/src/lib.rs:21412`, call at `:41086`), and that entry point
  > is replaced by a **`#[cfg(not(feature = "synthetic-jdk"))]` no-op shim** at
  > `vm/src/native/builtins.rs:29`. In both shipping binaries the identity
  > bindings are not out-voted, they are **compiled out**, so
  > `native-collections`' genuine `unmodifiable*` wrappers
  > (`native-collections/src/lib.rs:50688-50727`, in their own
  > `set_category(SyntheticStub)` window, plus `unmodifiableList` at `:15381`)
  > are the only registrations — and being `SyntheticStub`, they are themselves
  > dropped under `--jdk-only`, leaving real bytecode. That is the mechanism
  > behind the 37 byte-identical rows.
  >
  > **A `--features synthetic-jdk` binary HAS since been built** —
  > W7-50-synthetic-jdk-strict-six.md, measured **63 passed / 7 failed** on the
  > strict corpus. What has still never been done is running one in the
  > `--synthetic-jdk` **runtime mode**, which is the only place this residual
  > exists. Feature and mode are different things, and W7-50 §0 exists because
  > conflating them is what produced its six defects.
  >
  > **The repair, if the measurement confirms it:** delete the five
  > `unmodifiable*` identity registrations in
  > `phases_early::register_collections_extras_natives` and the three in
  > `phases_early::register_core_stdlib_extras` (`phases_early.rs:2204-2222`),
  > leaving `native-collections`' wrappers to win. Both shipping modes are
  > unaffected by construction — the code is not compiled into them.
  > Deliberately not done in the lane that verified this: `phases_early.rs`'s
  > collections registrars are in W7-50's blast radius, and an uncompiled edit
  > to the one arm nobody can build is how the 48/6 figure went stale in the
  > first place. **This residual is also a vacuous-green trap in the other
  > direction** — a probe run under `--synthetic-jdk` reports
  > `class=java.util.HashSet add=SUCCEEDED`, which reads exactly like an
  > unrelated JCA fix having failed to land; the trap is written into
  > `native-builtins/src/jca/provider_chain.rs` above `wrap_unmodifiable` and
  > into W7-63-jca-advertise-vs-serve.md §8.
* **Cannot adjudicate without a run — two.**
  1. The `Phaser` fix: `cargo build --features synthetic-jdk`, then the 2-party
     arrive/`getUnarrivedParties` race described below. This is still the
     record's own reason to treat that fix as unproven.
  2. §3's falsifier: `target/release/cratonvm --jdk-only -cp
     regression-suite/build RJdkForkJoin`, reaching `parallelStreams()`'s
     remaining assertions and exiting 0.
* **One stale hazard note:** the record warns that
  `regression-suite/src/RJdkPhaser.java` and `RJdkFieldModule.java` are
  untracked. They are tracked in this worktree now; `git status` is clean apart
  from this reconciliation.

Filed 2026-08-07 (JDK-only wave 2, lane W6-12). Companion to
`W6-4-duplicate-registration-gate.md`, whose static census produced the brief.

> **UPDATED 2026-08-11.** The `Collections` fidelity residual this record was
> kept for is re-measured on a built binary and is **structurally confined to
> `synthetic-jdk`**: both shipping modes drop the contested factories and run
> real `java.util.Collections` bytecode, and all 37 rows of
> `probes/JdkOnlyCollectionViewProbe` are byte-identical across HotSpot,
> `--real-jdk` and `--jdk-only`. A `java.util.concurrent` defect of this
> record's own shape was found while measuring it — `ForkJoinPool.commonPool()`
> allocates a factory class **JDK 25 does not declare**, which fails loudly
> under `--jdk-only` (`RJdkForkJoin`) and quietly in `--real-jdk` — and is
> filed with its patch in the 2026-08-11 section at the end.

## What the census got wrong

The duplicate-registration census counts `r.register(...)` call sites inside a
registrar function. It does not ask whether the **function is reachable**. Both
of this lane's headline pairs were already neutralised at the *call site*, which
is invisible to a per-function census:

| Registrar | Call site in `register_collections_natives` | Live? |
|---|---|---|
| `native-collections::register_stamped_lock_natives` | `let _ = register_stamped_lock_natives;` (line ~2449) | **No** — disabled 2026-07-28 |
| `native-collections::register_phaser_natives` | `#[cfg(feature = "synthetic-jdk")] register_phaser_natives(registry);` | **Yes**, and wrongly so |
| `native-collections::register_scheduled_executor_natives` | none — the function has no caller in the crate | **No** — dead since at least 2026-06 |
| `native-collections::register_collections_extras_natives` | ungated call (line ~2382) | **Yes**, and benignly so |

A census column that would have caught this: *is the registrar reachable from
`vm_init` in the configuration under test*. Reachability, not textual presence.

## StampedLock — no defect, and the record of why

`native-collections`' copy lost deliberately. The comment at its dead call site
records the A/B that killed it (4 threads incrementing under `writeLock()`, max
threads observed inside the critical section; HotSpot 25 = 1):

    default real-JDK mode : 335 threads inside, 345 lost updates
    --synthetic-jdk       :   2 threads inside,   1 lost update

Two independent defects: `native_sl_write_lock` gives up after `SL_SPIN_LIMIT`
(1000) yields and returns stamp `0` while the caller believes it holds the lock
(that is `tryWriteLock`'s failure contract, not `writeLock`'s); and it keeps
state in `set_field(this, 0, Value::Int)` **by index**.

`javap -p java.util.concurrent.locks.StampedLock` (JDK 25) — the real instance
fields, in declaration order:

    head           java.util.concurrent.locks.StampedLock$Node   (volatile transient)
    tail           java.util.concurrent.locks.StampedLock$Node   (volatile transient)
    readLockView   StampedLock$ReadLockView                      (transient)
    writeLockView  StampedLock$WriteLockView                     (transient)
    readWriteLockView StampedLock$ReadWriteLockView              (transient)
    state          long                                          (volatile transient)
    readerOverflow int                                           (transient)

Slot 0 is `head`, a `StampedLock$Node` **reference** — not `state`, and not an
int. (The disabled call site's comment says slot 0 is `state`; that is wrong in
detail and right in consequence. Either way the `Value::Int` write is coerced
and the `match { Value::Int(v) => v, _ => 0 }` read-back is permanently 0, so
the lock reads as free and every thread enters.) The surviving
`native-builtins/src/util_concurrent_ext.rs::register_stamped_lock_natives`
keys its state by a GC-stable identity in `crate::stamped_lock`, is
layout-independent, parks on a `Condvar`, and registers all 25 triples plus the
six view-class triples — the complete surface
`stampedlock-surface-must-be-complete-not-partial` asks for.

The three registration sites of that one function (`util_concurrent_ext.rs`
twice, `vm_init.rs` once) are the same implementation registered three times.
They are redundant, not conflicting; the only thing the duplication decided was
the ambient `NativeKind`, which that function now sets explicitly.

## Phaser — the live defect, two layers

`java/util/concurrent/Phaser`, nine triples contested. Real JDK 25 layout from
`javap -p java.util.concurrent.Phaser`:

    state   long                                    (volatile)
    parent  java.util.concurrent.Phaser             (final)
    root    java.util.concurrent.Phaser             (final)
    evenQ   AtomicReference<Phaser$QNode>           (final)
    oddQ    AtomicReference<Phaser$QNode>           (final)

Two registrars, same name, different crates:

* `native-builtins/src/phases_early.rs::register_phaser_natives` — **12**
  triples: `<init>()V`, `<init>(I)V`, `register`, `arrive`,
  `arriveAndAwaitAdvance`, `arriveAndDeregister`, `getPhase`,
  `getRegisteredParties`, `getArrivedParties`, `getUnarrivedParties`,
  `isTerminated`, `forceTermination`. State lives in an `int[3]` **holder
  object** stored in slot 1 (`parent`, a reference slot, so the array survives
  descriptor-aware coercion). `NativeKind::Intrinsic`.
* `native-collections/src/lib.rs::register_phaser_natives` — **9** triples, a
  proper subset of the twelve. State in object slots 0/1/2 as raw `Value::Int`
  (`PH_FIELD_PARTIES`/`ARRIVED`/`PHASE`). `NativeKind::Bridge`.

Overlap: **9 of 12**, i.e. all nine of the collections copy.

**Layer 1 — split-brain under `synthetic-jdk`.** `vm_init` calls
`register_builtins` (→ `register_synthetic_overrides` →
`register_phase51_natives` → the 12-triple holder version) and *then*
`register_collections_natives`. Last-write-wins, so the 9-triple slot version
won those nine and the holder version was left serving only
`getUnarrivedParties`, `isTerminated` and `forceTermination` — over storage the
winner never writes. Worse than a read divergence: `ph_holder` finds slot 1
holding an `Int` rather than an object, takes its legacy-migration path, and
**writes the new `int[3]` into slot 1**, destroying the `arrived` counter the
winning `arrive()` had been keeping there. One `getUnarrivedParties()` call
corrupts the phaser for every subsequent `arrive()`.

**Layer 2 — the `cfg` gate answered the wrong question.** The gate added by
`gap-phaser-real-bytecode-state.md` was `#[cfg(feature = "synthetic-jdk")]`
— a *build*-time predicate standing in for `config.use_synthetic_jdk`, a
*runtime* one. In a `synthetic-jdk` **feature** build running real-JDK **mode**,
`vm_init` takes the arm that skips `register_builtins` but still calls
`register_collections_natives`: the cfg was satisfied, the 9 slot-index natives
registered, and the synthetic 3-int layout shadowed real `Phaser` bytecode over
the real 5-field layout — which is precisely the regression the gate was added
to prevent. (See `syn-feat≠syn-MODE`.) The default build never had
`synthetic-jdk`, which is why nothing failed in the shipping configuration.

**Fix.** Drop the call site entirely — `let _ = register_phaser_natives;`,
matching the `StampedLock` and `ConcurrentSkipListMap` precedent in the same
function. The function is kept, not deleted, per the standing rule against
deleting a synthetic method. Result per configuration:

| Configuration | Before | After |
|---|---|---|
| no `synthetic-jdk` (default / shipping) | neither registrar runs → real bytecode | unchanged |
| `synthetic-jdk` feature, synthetic mode | 9 slot-index + 3 holder, mutually corrupting | 12 holder triples, one implementation |
| `synthetic-jdk` feature, real-JDK mode | 9 slot-index natives shadow real bytecode | no native shadow → real bytecode |

Synthetic mode keeps working because the fabricated `Phaser` gets
`instance_fields(3)` in `classloading/src/class_manager.rs`, and
`instance_fields` declares every slot `Ljava/lang/Object;` — so the holder
*reference* in slot 1 is the type-correct write for that layout and the raw
`Value::Int` writes were the type-incorrect ones.

## The other two flagged pairs

* `register_scheduled_executor_natives` (`native-collections` line ~50492 vs
  `phases_early` line ~10144): the `native-collections` copy has **no caller**
  and has been dead long enough to be named in two review documents.
  `register_collections_natives` calls the differently-named
  `register_executors_scheduled_natives` instead. **Inert.** Left alone.
* `register_collections_extras_natives` (`native-collections` line ~48041 vs
  `phases_early` line ~100): 16 genuinely contested triples on
  `java/util/Collections` — `unmodifiable{Set,Map,SortedSet,Collection}`,
  `synchronized{List,Set,Map,Collection}`, `singleton`, `singletonMap`,
  `frequency`, `nCopies`, `min`, `max`, `swap`, `fill`. All **static**, so
  there is no receiver and no shared per-instance state: this is a fidelity
  question, not a split-brain. `native-collections` wins, and it is the copy
  that should: it returns real wrappers (`native_collections_unmodifiable_set`)
  where `phases_early` returns `native_return_first_arg`. Left alone.

  One residual worth recording rather than fixing blind: the family is served
  by *both* copies at different fidelities. `unmodifiableSet` gets a real
  wrapper from `native-collections`; `unmodifiableList`, `checkedList`,
  `rotate`, `copy`, `replaceAll`, `list` and `singletonList` are registered
  only by `phases_early` and several are `native_return_first_arg`. So
  `Collections.unmodifiableSet(s).add(x)` throws while
  `Collections.unmodifiableList(l).add(x)` succeeds. Synthetic-jdk only.

## Falsifier

For the fix: build with `--features synthetic-jdk` and, in synthetic mode, run a
`Phaser` with 2 registered parties where one thread calls `arrive()` and another
calls `getUnarrivedParties()` between arrivals. Before: `getRegisteredParties()`
and `getArrivedParties()` go to zero after the first `getUnarrivedParties()`
call. After: the counts stay consistent and `arriveAndAwaitAdvance` advances the
phase. If the post-fix run instead shows every accessor answering 0, the
`native-builtins` holder implementation is not reachable in that configuration
and the call site must be restored rather than removed.

> **UPDATED 2026-08-07 — a `Phaser` vector now exists, and it is untracked.**
> `regression-suite/src/RJdkPhaser.java` is in the working tree,
> `regression-suite/run.sh` lists `RJdkPhaser` in `JDKONLY_CLASSES`, and
> `run.sh`'s own coverage comment cites it (*"This is how RJdkPhaser — 240
> checks — arrived inert"*). But `git ls-files` does not know the file:
> `git status` reports `?? regression-suite/src/RJdkPhaser.java` (same for
> `RJdkFieldModule.java`). **On a fresh clone `run.sh` schedules two classes
> whose sources do not exist**, so the paragraph below is still true of CI even
> though it is no longer true of this worktree. Track the file before treating
> the vector as coverage. This is the same untracked-fixture hazard as
> `probes/BdProbe.java` —
> [§8 and §9 of *Natives over real JDK
> classes*](../../architecture/natives-over-real-jdk-classes.md).

---

## 2026-08-11 — the `Collections` fidelity residual, re-measured, and a third instance of this record's habit

This record was kept by a records audit for its *"`Collections` fidelity
split"* paragraph. That paragraph is re-measured below, and while measuring it
a `java.util.concurrent` defect of exactly this record's shape turned up — a
registrar naming a class the image does not declare — so it is filed here with
its patch.

Everything below was measured on the built `target/release/cratonvm.exe` of the
wave-1 integration merge against Temurin 25.0.3.9 (windows/x64), one binary,
three arms, only the mode differing. **Nothing was rebuilt afterwards**, so the
patch in §3 is unverified source.

### 1. The `Collections` fidelity residual is narrower than it reads

The residual says the `unmodifiable*` family is served by *both* copies at
different fidelities, so `unmodifiableSet(s).add(x)` throws while
`unmodifiableList(l).add(x)` succeeds — **"Synthetic-jdk only"**.

That scoping is now structural rather than incidental, and it is worth writing
down because it changes who can ever see the bug. The six `unmodifiable*`
factories in `native-collections`' `register_collections_extras_natives` sit in
their own `set_category(NativeKind::SyntheticStub)` window, so under
`--jdk-only` they are **dropped at registration** and real
`java.util.Collections` bytecode runs. Measured: all 37 rows of
`probes/JdkOnlyCollectionViewProbe` — the whole `unmodifiable*` / `List.of` /
`Map.of` / `copyOf` / sub-list / comparator / `Function` surface — are
byte-identical across HotSpot 25, `--real-jdk` and `--jdk-only`, including
`unmod.list.throws=UnsupportedOperationException`, the exact asymmetry the
residual describes.

So: **the residual cannot be reached in either shipping mode.** It survives
only in a `synthetic-jdk` build, which has no class library to fall back to —
and that arm is still unmeasured here, because this lane had no such binary.
Do not read the green above as closing it; read it as bounding it.

### 2. Third instance of the habit this record is kept for

The habit is: *the brief was refuted on its primary claim and confirmed on its
sibling.* Two more instances landed the same day, both in
`native-collections/src/lib.rs`, both in-file comments that argued a fallback
could not exist and were outlived by later work in their own file — the
`ConcurrentHashMap$KeySetView` iterator, and a pair of `LinkedList` registrar
comments asserting that the real `first`/`size` fields are never written. Both
are written up in
docs/known-issues/jdk-only/W2-1-strict-refuses-the-synthetic-stream-stack.md.
The generalisation, stated once: **a comment that explains why something is
impossible ages worse than a comment that explains what something does**, and
it ages invisibly, because nothing executes it.

### 3. `ForkJoinPool.commonPool()` names a factory class JDK 25 does not declare

`RJdkForkJoin --jdk-only` dies on its first `ForkJoinPool.commonPool()`:

    CK RJdkForkJoin sum=199990000 fill=24995000
    CK RJdkForkJoin leaves=64 completions=127
    Exception in thread "main" java/lang/NoClassDefFoundError:
        java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory
        at RJdkForkJoin.parallelStreams(RJdkForkJoin.java:209)

It fails identically on a pre-merge control binary, so it is standing work, not
a regression. The three arms of
`ForkJoinPool.commonPool().getFactory().getClass().getName()`:

| arm | answer |
|---|---|
| HotSpot 25 | `java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory` |
| `cratonvm --real-jdk` | `java.util.concurrent.ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory` |
| `cratonvm --jdk-only` | `NoClassDefFoundError` on that name |

`javap -p 'java.util.concurrent.ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory'`
on JDK 25 answers *class not found*; the sibling
`…$DefaultForkJoinWorkerThreadFactory` is there. So the hard-coded default in
`resolve_common_factory_internal_name` is a **JDK-21-era name**:
`alloc_common_factory`'s `ensure_class_initialized` misses, the `Err(_)` arm
fabricates the class, and `--jdk-only` refuses the fabrication — §5 enforcing
correctly on a name the VM invented for this image.

Note what the middle row means on its own: `Compatible` is not right here
either, it is merely quiet. It hands back an instance of a class the running
image does not declare.

#### Out-of-file patch — SUPERSEDED, DO NOT APPLY

> **Reconciled 2026-08-12.** The observation was right and was acted on; both
> prescribed variants were considered and **rejected on the merits**. The defect
> is fixed by commit `46bb0ad2e`, which resolves the factory from the image's
> own public static field (`common_factory_from_image`,
> `native-builtins/src/phases_late/concurrent.rs:8524-8538`) rather than naming
> any nested class. The conservative variant's ordering is explicitly refused in
> a comment at `concurrent.rs:8566-8577` — probing the policy before
> `try_alloc_concurrent_synthetic` would mint the class one call earlier. The
> fidelity variant would have changed `--real-jdk`, which this record itself
> said was out of lane. Write-up: W7-14-fjp-common-factory-bound-by-name.md.

**Owner: `native-builtins/src/phases_late/concurrent.rs`** (this lane owns
`native-collections/src/lib.rs` and this record only). Function
`alloc_common_factory`; nothing else in the file changes.

Conservative variant — **`--jdk-only` only, `Compatible` byte-for-byte
unchanged**. This is the idiom W2-1 established for `cratonvm/stream/LazyOp`:
ask the policy before minting, and treat a refusal as "try the real class"
rather than as an error. In `Compatible` the added
`try_ensure_synthetic_class(&target, 1)` is byte-for-byte the call
`try_alloc_concurrent_synthetic` makes on the next line, so the observable
behaviour and the fabricated class are identical.

```rust
pub(crate) fn alloc_common_factory(ctx: &mut dyn NativeContext) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let target = resolve_common_factory_internal_name(ctx);
    match ctx.ensure_class_initialized(&target) {
        Ok(cid) => {
            let nfields = ctx.class_num_total_fields(cid).max(1);
            Ok(ctx.alloc_object(cid, nfields))
        }
        Err(_) => {
            // The default this function asks for is a JDK-21-era name: JDK 25
            // declares only `ForkJoinPool$DefaultForkJoinWorkerThreadFactory`,
            // and HotSpot 25 answers `commonPool().getFactory().getClass()
            // .getName()` with it. Fabricating the missing name is what
            // `--jdk-only` refuses, and that refusal costs the whole call —
            // `RJdkForkJoin` dies at `parallelStreams():209` on the first
            // `ForkJoinPool.commonPool()`, not on anything about factories.
            //
            // Ask the policy first, exactly as `stream_make_lazy_derived`
            // does: only when the fabrication is REFUSED do we go looking for
            // the sibling this image actually declares. In `Compatible` the
            // probe succeeds and the line below fabricates as it always has,
            // so that mode does not move.
            if ctx.try_ensure_synthetic_class(&target, 1).is_err() {
                const IMAGE_DEFAULT: &str =
                    "java/util/concurrent/ForkJoinPool$DefaultForkJoinWorkerThreadFactory";
                if let Ok(cid) = ctx.ensure_class_initialized(IMAGE_DEFAULT) {
                    let nfields = ctx.class_num_total_fields(cid).max(1);
                    return Ok(ctx.alloc_object(cid, nfields));
                }
            }
            try_alloc_concurrent_synthetic(ctx, &target, 1)
        }
    }
}
```

Fidelity variant — same fix, but it also makes `--real-jdk` agree with
HotSpot. Replace the body with an image-driven candidate list:

```rust
    let target = resolve_common_factory_internal_name(ctx);
    for name in [
        target.as_str(),
        // JDK 21 declares this one; JDK 25 does not.
        "java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory",
        // JDK 25's, and the one HotSpot 25 reports from getFactory().
        "java/util/concurrent/ForkJoinPool$DefaultForkJoinWorkerThreadFactory",
    ] {
        if let Ok(cid) = ctx.ensure_class_initialized(name) {
            let nfields = ctx.class_num_total_fields(cid).max(1);
            return Ok(ctx.alloc_object(cid, nfields));
        }
    }
    // Only a synthetic-JDK build reaches here: no image, so the fabrication is
    // the whole implementation and is permitted.
    try_alloc_concurrent_synthetic(ctx, &target, 1)
```

**State the cost of the fidelity variant plainly, because it breaks a standing
constraint:** on JDK 25 it changes what `--real-jdk` answers for
`commonPool().getFactory().getClass().getName()`, from the invented
`…$DefaultCommonPool…` to HotSpot's `…$Default…`. That is a Compatible-mode
behaviour change. It is the right answer and it is not this lane's to take.

Neither variant touches the operator-supplied path: a loadable class named by
`java.util.concurrent.ForkJoinPool.common.threadFactory` is still the first
candidate and still wins, so the Keycloak/Quarkus check
`getFactory().getClass().getName().equals(property)` behaves as before.

**Falsifier.** Under `--jdk-only`, `RJdkForkJoin` should reach
`parallelStreams()`'s remaining assertions and exit 0, and
`FjpFactoryProbe`-style output should print a class name rather than throw. If
it instead throws `NoClassDefFoundError` on
`…$DefaultForkJoinWorkerThreadFactory`, the sibling is not on the boot
classpath for this call and the candidate list is not the problem — the
resolution context is.

---

~~No regression-suite vector covers `Phaser`~~ — `RJdkAqs` is the only *tracked* vector that
names anything in this area and it exercises `StampedLock` only (lines 327-341:
`tryOptimisticRead`/`validate`/`isWriteLocked`/`isReadLocked`), on the
`native-builtins` path that was never in contention. That absence is why the
defect survived, and it is the main reason to treat this fix as unproven until
someone runs the probe above.

---

## The `Collections` fidelity residual, adjudicated — 2026-08-12 (lane A31)

The residual said, twice (§"the `Collections` fidelity split" and §1 of the
2026-08-11 re-measurement):

> `Collections.unmodifiableSet(s).add(x)` throws while
> `Collections.unmodifiableList(l).add(x)` succeeds. **Synthetic-jdk only.**

A `--features synthetic-jdk` binary was built and launched with
`--synthetic-jdk`. **That claim is false.** Both throw:

```
--synthetic-jdk:
  R unmod.set.add  = UnsupportedOperationException (correct)
  R unmod.list.add = UnsupportedOperationException (correct)
  R unmod.coll.add = UnsupportedOperationException (correct)
  R unmod.map.put  = UnsupportedOperationException (correct)
  R unmod.list.setThrows = UnsupportedOperationException (correct)
  R unmod.sortedSet.add  = UnsupportedOperationException (correct)
  R unmod.list.writeThrough = viewSize=2          (HotSpot: 2)
  R unmod.set.cls  = java.util.Collections$UnmodifiableSet
  R unmod.coll.cls = java.util.Collections$UnmodifiableCollection
  R unmod.map.cls  = java.util.Collections$UnmodifiableMap
```

`unmodifiableList` even returns a real wrapper (`sameObject=false`,
`java.util.Collections$UnmodifiableList` — HotSpot names the
`…$UnmodifiableRandomAccessList` subclass, a cosmetic difference), so
`native_return_first_arg` is **not** what serves it. The whole `unmodifiable*`
family is correct in the one mode the residual claimed it was broken in.

**But the residual pointed at the right file and the wrong members.** Six other
rows of the same `Collections` factory surface are wrong in `--synthetic-jdk`,
and every one of them is green on HotSpot 25, on `--jdk-only`, and on the
feature binary under `--jdk-only` — so all six are genuinely synthetic-mode
only, exactly the scoping the residual asserted:

| row | HotSpot / `--jdk-only` | `--synthetic-jdk` |
|---|---|---|
| `Collections.emptyList().add("b")` | `UnsupportedOperationException` | **`ADDED size=1`** — the shared empty list is mutable |
| `Collections.singletonList("a").add("b")` | `UnsupportedOperationException` | **`ADDED size=1`** |
| `Collections.singletonMap("k","v").put("x","y")` | `UnsupportedOperationException` | **`PUT size=1`** |
| `Collections.checkedList(new ArrayList<String>(), String.class)` then `.add(Integer)` | `ClassCastException`; wrapper is `…$CheckedRandomAccessList` | **`ADDED size=1`**; wrapper is **`java.util.ArrayList`** — no wrapper at all |
| `Collections.copy(dst, src)` | `[7, 8, 9]` / `[4, 5]` | **`[0, 0, 0]` / `[0, 0]`** — a silent no-op at both unequal and equal length |
| `Collections.copy(shortDst, longSrc)` | `IndexOutOfBoundsException` | **`COPIED [0]`** — no bounds check either |

Plus two absences, which are honest failures rather than wrong answers:

```
R synchronizedList.add ! java.lang.NoSuchMethodError:
    java.util.Collections$SynchronizedList.<init>(Ljava/util/List;)V
R singleton.set.add    ! java.lang.NoSuchMethodError:
    java.util.Collections$SingletonSet.add(Ljava/lang/Object;)Z
R collections.enumeration ! java.lang.NoSuchMethodError:
    java.util.Arrays$ArrayList.enumeration(Ljava/util/Collection;)Ljava/util/Enumeration;
```

(The last one is the argument-0 misnaming trap: `enumeration` is a **static** on
`java.util.Collections` and `java.util.Arrays$ArrayList` never declared it. The
class in a synthetic-mode `NoSuchMethodError` is the runtime class of `arg0`
when the method is genuinely absent — do not chase the named class.)

Green in `--synthetic-jdk`, recorded so the list is not re-derived:
`rotate`, `replaceAll`, `fill`, `swap`, `reverse`, `min`, `max`, `frequency`,
`nCopies`, `singletonList` contents, `List.of(...).add` → UOE.

### What this changes

1. **The residual is not retired.** Its literal claim is falsified; the surface
   it named is worse than it said. It should be rewritten around the six rows
   above rather than closed.
2. **`Collections.copy` is the dangerous one** and belongs in the
   `W7-20`/`W7-1` species, not here: it writes nothing, throws nothing, and
   returns normally, so a caller that copies-then-reads gets zeros. That is the
   fabricated-success shape, in a mode with no real bytecode to fall back to.
3. **`emptyList()` being mutable is a shared-singleton hazard**, not a fidelity
   nit: `Collections.emptyList()` is process-wide, so one `add` poisons every
   later caller in the VM.
4. The residual's *scoping* claim — "synthetic-jdk only" — **held for all six**.
   That part was right, and the 2026-08-11 re-measurement's argument for it
   (`SyntheticStub`-tagged factories are dropped at registration under
   `--jdk-only`) is confirmed by the `--jdk-only` column.

See lane A31's NOMINATION A31-4.

### The W7-78 amendment's prediction is also contradicted

The 2026-08-12 W7-78 block above (in the bullet list at the head of this record)
makes two forward statements that the run settles:

1. > "a probe run under `--synthetic-jdk` reports
   > `class=java.util.HashSet add=SUCCEEDED`, which reads exactly like an
   > unrelated JCA fix having failed to land"

   **Measured: it does not.** `--synthetic-jdk` reports
   `unmod.set.cls = java.util.Collections$UnmodifiableSet` and
   `unmod.set.add = UnsupportedOperationException`. The vacuous-green trap
   written into `native-builtins/src/jca/provider_chain.rs` above
   `wrap_unmodifiable`, and into W7-63 §8, is **guarding against a shape that
   does not occur**. Neither comment is harmful, but neither should be cited as
   a known hazard, and W7-63 §8's reading of a `HashSet`-shaped result should not
   be trusted as a synthetic-mode signature.

2. > "**The repair, if the measurement confirms it:** delete the five
   > `unmodifiable*` identity registrations in
   > `phases_early::register_collections_extras_natives` and the three in
   > `phases_early::register_core_stdlib_extras`"

   **The measurement does not confirm it.** The `phases_early` identity bindings
   are not the ones answering in `--synthetic-jdk` — a real wrapper class is
   returned and it refuses mutation. Deleting them would be a change with no
   measured effect in any of the three configurations. Do not take that repair
   on this record's authority; the members that are actually broken are
   `emptyList`, `singletonList`, `singletonMap`, `checkedList` and
   `Collections.copy`, and none of them is on that deletion list.

Method note, because it is what made both contradictions visible: **non-null is
not the contract.** Every row above asserts the thrown type, the wrapper class
name, and the post-condition contents — `unmod.list.identity` asks
`sameObject`, `collections.copy.equalLen` reads the destination back. A probe
that only checked "a Set came back" would have agreed with both predictions.
