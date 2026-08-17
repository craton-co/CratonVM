# `TestMultiThread` — `NullPointerException: Cannot read the array length because "<local4>" is null` during concurrent connection open

## Status
**FIXED 2026-08-16**, on branch `h2ki-20260816` off `origin/dev` @ `ecc09d40d`,
Azure host `azureuser@20.80.105.49`.

The original report had the shape of the bug right — *"a shared array field being
read by one thread before another has finished initializing it"* — and the layer
wrong. There is no H2 field involved, and no CratonVM memory-model gap. The null
array was a **`java.lang.String`'s `value` slot, nulled by the GC's own
reference-processing pass through an address that had been reclaimed and
reused.** The NPE surfaces in whatever frame next touches that String, which is
why the JDK's helpful-NPE machinery could only name it `<local4>`.

A residual with a different mechanism survives and has its own record — see
"What this does NOT close".

## How it was found: the same defect, three louder witnesses

Reproducing the reported NPE first produced three *other* failures of the same
class, each of which names the mechanism outright:

```
ClassCastException: class java.lang.String cannot be cast to class
  org.h2.util.CloseWatcher
    at org/h2/util/CloseWatcher.pollUnclosed(CloseWatcher.java:59)
    at org/h2/jdbc/JdbcConnection.closeOld(JdbcConnection.java:203)
    at org/h2/jdbc/JdbcConnection.<init>(JdbcConnection.java:137)

ClassCastException: class java.lang.String cannot be cast to class
  sun.nio.ch.FileLockTable$FileLockReference
    at sun/nio/ch/FileLockTable.removeStaleEntries(FileLockTable.java:236)

ClassCastException: class org.h2.expression.analysis.WindowFrameUnits cannot be
  cast to class sun.nio.ch.FileLockTable$FileLockReference        (single-threaded TestScript)
```

All three are `(SomeReferenceSubclass) queue.poll()` on a `java.lang.ref.ReferenceQueue`.
Two are in the JDK, one is in H2, one is not even concurrent — so this was never
an H2 race, and `new JdbcConnection(...)` is only where it shows up because
`closeOld()` polls a `ReferenceQueue` on every connection open.

## Root cause — two independent defects, both in reference processing

### 1. `ReferenceQueue.poll` / `Reference.enqueue` had no mutual exclusion

The real JDK guards `head`, `queueLength` and `Reference.next` with
`ReferenceQueue.lock`, a `ReentrantLock` taken by `enqueue0`, `poll` and
`remove` alike. CratonVM's natives (`native-builtins/src/reference.rs`) REPLACE
that bytecode and supplied no exclusion of their own: `native_rq_poll` read
`head`, read `head.next`, then wrote both back with nothing stopping a second
thread from reading the same `head` in between. Two threads then popped the same
Reference and both returned it; other interleavings dropped whole segments of the
list.

Measured, 8 threads × 400 phantom references through one queue, against
HotSpot's 3200-created / 3185-delivered / 0-duplicate baseline:

| | delivered | distinct | duplicates |
|---|---|---|---|
| HotSpot 25 | 3185 | 3185 | 0 |
| pristine `dev` | 3114 | 3106 | 8 |
| fixed | 3197 | 3197 | **0** |
| fixed, on the merged tree under load | 3127 | 3126 | 1 |

The same probe run single-threaded is identical on all three VMs, which is what
first separated this half from the second.

The last row is not noise to wave away: a duplicate can still occur, at roughly
one in three thousand instead of one in four hundred, and it is the *other*
residual — same-class reuse, which a class-shape guard cannot see by
construction. It is recorded with the MVStore residual below.

**Fix:** `with_queue_monitor` — every queue-list mutation in
`native_rq_poll` and in both arms of `native_ref_enqueue` now runs under the
**`ReferenceQueue` object's own Java monitor**. Per-queue, no new static and no
new `Mutex` (the native-builtins lock ratchet counts those), and no Java code
competes for it: JDK 9+ `ReferenceQueue` synchronizes on a private
`ReentrantLock` field, never on `this`. The JDK's lock therefore nests strictly
inside ours on the delegating enqueue arm and never in the other order.
`remove()`/`remove(long)` inherit the exclusion because they poll in a loop.

### 2. The GC's reference-processing pass wrote through reclaimed addresses

`process_references_after_gc` (and its G1-remark twin) works from PRE-GC
addresses that it relocates through `pointer_map`, and its only shape test on the
object at the far end was `num_fields >= 2`. That is a coincidence, not a check:
a `java.lang.String` has four (`value`, `coder`, `hash`, `hashIsZero`) and
passes; so does nearly every class. When a Reference had been reclaimed and its
slot reused, the loops still wrote:

* the **enqueue** loop published the reusing object as the queue head — the three
  `ClassCastException`s above;
* the **cleared** loop nulled field 0 — on a `String` that is `value`, a
  `byte[]`, and the array that vanishes is **this record's headline NPE**;
* the **weak/phantom restore** loop wrote a referent into field 0 of the same.

**Fix:** a class-shape guard in `vm/src/runtime/interpreter/gc_and_alloc.rs`.
Everything on those lists is a `java.lang.ref.Reference` and a
`java.lang.ref.ReferenceQueue` by construction, so anything else at the recorded
address is proof the address no longer names what the processor recorded. The
`ClassManager` read guard is taken before the reference-processor mutex (L10
before L7) and held across the loops rather than reacquired inside them. A class
that is not loaded means no such object can exist, so the guard admits — it must
never be the thing that silently stops reference processing on a stripped image.

### 2b. The cleaner drain, the last field-0 writer

The headline NPE outlived (1) and (2) and stayed reproducible under
`-XX:+UseGenerationalGC`, which separated a third producer:
`run_cleaner_actions_impl` sets slot 1 (the cleaned flag) and NULLS slot 0 (the
action) on every address it dequeues, before invoking anything — the same
field-0 null on the same reused `String`. `is_cleanable_shaped` now screens both
the submit and the drain: a pending cleaner action must be a
`java.lang.ref.Reference` subclass (the real JDK's `jdk.internal.ref.Cleaner` and
`PhantomCleanable` both are) or a `java.lang.ref.Cleaner$Cleanable` implementor
(the synthetic shape `phases_late`/`servlet` build). The drain re-checks rather
than trusting the submit's verdict, because the queue survives collections
between the two.

## Verification

`org.h2.test.db.TestMultiThread`, `--nojit --Xmx 1g`, real-JDK mode, ABBA-
interleaved on one host in one session so load cannot favour an arm
(`A B B A A B B A`; `A` = pristine `origin/dev` @ `ecc09d40d`, `B` = the same
tree with fixes 1 and 2):

| arm | runs | result |
|---|---|---|
| A (pristine dev) | 4 | **0 pass.** 3× `ClassCastException` out of `ReferenceQueue.poll()`; 1× the MVStore residual below |
| B (fixed) | 4 | **3 pass.** 1× the MVStore residual below — and no `ReferenceQueue` signature in any run |

A second, 12-run interleave adding fix 2b and a collector axis
(`A C D E HS C D A E C D HS`; `C` = fixed/ZGC, `D` = fixed/`-XX:+UseGenerationalGC`,
`E` = pristine/`-XX:+UseGenerationalGC`):

| arm | pass | fail | 900 s timeout |
|---|---|---|---|
| C (fixed, ZGC) | 3 | 0 | 0 |
| D (fixed, Generational) | 2 | 0 | 1 |
| A (pristine, ZGC) | 1 | 1 | 0 |
| E (pristine, Generational) | 1 | 0 | 1 |
| HotSpot 25 | 2 | 0 | 0 |

The host was heavily loaded for that sequence (load average 8–13, ~20 other
sessions) and it did **not** reproduce the base defect on either pristine arm —
which is exactly why the ABBA table above, taken on a quieter host, is the
judge and this one is only corroboration. The two timeouts are the suite's known
load-flip behaviour, one on each side of the fix. Every fixed-binary run
completed clean; the census

```bash
grep -c "cannot be cast to class org.h2.util.CloseWatcher\|cannot be cast to class sun.nio.ch.FileLockTable" err.log
grep -c "Cannot read the array length" err.log
```

is **0 in every fixed-binary run of both sequences**, and the second grep — this
record's headline — last read non-zero on a fixed binary *before* 2b, under
`-XX:+UseGenerationalGC`. HotSpot 25 passed the class in every control run.

Rust gates on the merged tree: `cargo test -p cratonvm-native-builtins --lib`
3565 passed / 0 failed; the blocking synthetic-jdk gate
(`--lib --features synthetic-jdk`) 3740 passed / 0 failed;
`cargo test -p cratonvm-vm --lib` 2512 passed / 0 failed; `stub_ratchet` green;
`cargo check --workspace --features synthetic-jdk` clean.
`lock_discipline_ratchet` is red — 455 raw lock constructions against a baseline
of 432 — and that is pre-existing: pristine `origin/dev` measures **456** in the
same session. This change adds no `Mutex::new`/`RwLock::new` at all; the queue
exclusion deliberately reuses the `ReferenceQueue` object's Java monitor for
that reason.

The synthetic reference-queue probe (`8 threads × 400 phantom refs`, above) is
the cheaper regression tripwire and needs no H2: duplicates must be 0 and
delivered must be within a few of created.

## What this does NOT close

A residual that is **also present on pristine `origin/dev`** — the MVStore
background writer seeing an object of an unrelated class (`ValueDataType.write`
calling `toByteArray()` on what should be a `BigInteger` and is a
`java.lang.String`; or the same thread's `Value v` reading null). It has no
`java.lang.ref` frame anywhere in it and survives all three guards above. On the
pristine arm it is usually pre-empted by the louder reference-queue bug, which is
why it had not been seen alone before. It is tracked separately as
bug-h2-testmultithread-mvstore-writer-object-identity-FIXED-20260817, together
with the one part of the reference machinery a CLASS-shape guard provably cannot
cover: same-class reuse (2-5 references in 3200 still arrive as a
half-constructed instance of the same class).

**CLOSED 2026-08-17** by that page. The producer was
`Collections.synchronizedSet`, whose natives never took the wrapper's `mutex`:
H2 keeps `CloseWatcher.refs` in one, the racing `HashSet` lost entries, and a
`CloseWatcher` (a `PhantomReference`) therefore died while this processor was
still tracking it. The same-class hole is closed by an identity stamp — the
identity hash recorded at `discover_reference` — and the pre-GC referent-null
pass, which had no class-shape guard at all, now carries both.

## Repro (for a future regression)

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Not deterministic — run at least 6 times and read the two greps in the
"Verification" section rather than the pass/fail alone, since the residual above
fails the same class for an unrelated reason.
