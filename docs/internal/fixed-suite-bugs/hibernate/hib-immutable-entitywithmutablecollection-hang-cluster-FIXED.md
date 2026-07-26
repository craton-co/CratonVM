# Hibernate immutable entity-with-mutable-collection hang cluster — FIXED

| | |
|---|---|
| **Status** | ✅ RESOLVED — all documented classes complete on current CratonVM and HotSpot. |
| **Resolved** | 2026-07-15 on Azure `dev` descendant `1520b2b8`. |
| **Area** | `@Immutable` entities whose associations use mutable collections. |

## Scope correction

The original title said "17/17 classes" and then additionally named `ImmutableTest`.
Its explicit list actually contains **16** mutable-collection variants plus
`org.hibernate.orm.test.immutable.ImmutableTest`: **17 runnable classes**
total. The abstract shared bases remain expected `NOTESTS` classes and are not
part of the runnable set.

## Root cause

The shared failure mechanism was a moving-GC safety defect in the native
`ArrayList` implementation. `al_ensure_capacity()` allocates a replacement
backing array, but `ArrayList.add`/`addAll` previously retained the receiver,
the old backing array, or elements across that allocation without native-root
pins. A moving collection could therefore leave later field writes using stale
object handles, causing the real list's data/size to remain inconsistent. The
collection-heavy Hibernate fixture made this appear as a uniform startup/test
hang across association variants.

`8665d1adb5650a140d2ebd8faf4a46e291ccf5de`
(`fix(native-collections): pin ArrayList.add/addAll across ensure_capacity's
GC-triggering resize`) pins and rereads every object reference that remains
live across the allocating path. This closes the shared runtime hazard rather
than masking individual Hibernate variants.

## Validation

A fresh isolated release binary was built from
`/data/data/cratonvm-hib-immutable-mutablecoll-20260715` into the dedicated
`/data/data/cv-target-hib-immutable-mutablecoll-20260715` target, then copied
as `cratonvm-hib-immutable-mutablecoll-20260715`.

- CratonVM default JIT: each of the 17 classes was run in a **fresh process**
  with the original 300-second ceiling and `-Dcraton.batch=1`. Every process
  exited `0`; all **335 started tests passed** and the suite reported five
  expected skips. The slowest two failure-expected variants completed in
  132.467s and 137.551s, below the historical 300-second timeout.
- HotSpot JDK 25 control: the same 17-class list completed with **335 passed**
  and five expected skips, confirming the fixture itself is healthy.

No residual hangs, failures, crashes, or new issue documents remain for this
cluster.
