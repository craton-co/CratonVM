# StampedLock was already fixed; Phaser was the live split-brain

**Status:** the brief this lane was handed is **refuted on its primary claim**
and **confirmed on its sibling**. `StampedLock` needs no change — the losing
registrar was disabled on 2026-07-28 and the call site is a `let _ =`. `Phaser`
had the exact defect the brief described for `StampedLock`, plus a second one,
and is fixed here.

Filed 2026-08-07 (JDK-only wave 2, lane W6-12). Companion to
`W6-4-duplicate-registration-gate.md`, whose static census produced the brief.

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
`gaps/gap-phaser-real-bytecode-state.md` was `#[cfg(feature = "synthetic-jdk")]`
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

~~No regression-suite vector covers `Phaser`~~ — `RJdkAqs` is the only *tracked* vector that
names anything in this area and it exercises `StampedLock` only (lines 327-341:
`tryOptimisticRead`/`validate`/`isWriteLocked`/`isReadLocked`), on the
`native-builtins` path that was never in contention. That absence is why the
defect survived, and it is the main reason to treat this fix as unproven until
someone runs the probe above.
