# A `VarHandle` access with a null coordinate answers instead of throwing

## Status
**FIXED and CLOSED, 2026-09-02.** Opened 2026-09-01 while writing the hot
vector for the `VarHandle` reference-read bind. The page named three broken
shapes; the oracle sweep found **75**.

`RJdkVarHandleNullCoord` walks every access mode against an `int` instance
field, an `Object` instance field, an `int[]` element handle and a static-field
control — 76 rows. Against JDK 25:

| | rows disagreeing with HotSpot |
|---|---:|
| before | **74 of 76** |
| after | **0 of 74** (byte-identical; two rows moved to a separate rule, below) |

The one row that already agreed was the control: a static-field handle has no
coordinate, so there is nothing to be null.

## What it was

HotSpot raises `NullPointerException` for every access mode given a null
coordinate — the receiver of an instance-field handle, or the array of an
array-element handle. CratonVM answered instead:

```text
  35 rows  returned 0          primitive reads, compareAndExchange, getAndAdd, bitwise
  15 rows  returned false      every CAS mode
  13 rows  returned normally   every write mode
  12 rows  returned null       reference reads
   1 row   correct             the static-handle control
```

The **write** rows are the worst: a store through a null receiver was dropped
with no signal anywhere, so the next read of that field returned a stale value
that looks legitimate. The **CAS** rows are next: answering `false` tells the
caller "somebody else won the race", which turns a null receiver into a retry
loop or a silently-taken wrong branch rather than a failure. That is the same
species as
`a swallowed introspection error deletes every annotation` — a loud failure
converted into a quiet wrong answer.

## The fix

One guard, `vh_check_leading_coordinate`, in
`native-builtins/src/lang_invoke.rs`, called from the seven registered access-mode
entry points (`varhandle_get`, `_set`, `_compare_and_set`,
`_compare_and_exchange`, `_get_and_set`, `_get_and_add`, `_get_and_bitwise`).

**One file, because the registry said so.** `phases_late/reflect_invoke.rs`
registers many of the same names, and it would have been the natural place to
look. `--dump-native-registry` names `lang_invoke.rs` for **all 37**
`java/lang/invoke/VarHandle` registrations — the registry is FIRST-WINS and the
other file never owns a slot. Reading the source instead of the dump is how the
`AtomicReference.compareAndSet` work gated an inert copy and measured 400 000
invocations in *both* arms.

**Free on the hot path, and that is checkable rather than argued.** Only a
leading coordinate that is actually null (or absent) reaches the kind lookup;
everything else returns after one `match`. A primitive in `args[1]` — which is
what a static handle's `set` carries — takes the same early exit. And the JIT
thin direct binds never reach the native at all: with the guard in,
`HibfixVarHandleProbe` still reports `read served=2 000 000 declined=0`,
`write served=698 000 declined=0`, `CAS served=2 094 000 declined=0`, so
compiled code pays exactly nothing.

`CRATONVM_VH_NULL_COORDINATE_NPE=0` restores the old silence. It exists because
this converts silence into an exception on a path any workload can reach, so a
suite that starts failing has to be bisectable to the rule rather than to a
rebuild.

## Deliberately still open: the unsupported-mode rule

`getAndAdd*` and `getAndBitwise*` on a **reference** variable are a different
rule and are not fixed here. HotSpot answers `UnsupportedOperationException` —
the access-mode support check runs BEFORE the null check — and CratonVM now
answers `NullPointerException` for them.

That is a wrong exception CLASS, not a wrong outcome, and it is strictly better
than what those two rows did before (`returned:0` / `returned:null`). Making it
exact needs its own oracle sweep across every variable type, because the rule is
not uniform: `getAndBitwise*` IS supported for `boolean` while `getAndAdd*` is
not. Two rows, one measurement, and no reason to smuggle it into this one — the
same call this page made about the coordinate-COUNT rule when it was opened.

`RJdkVarHandleNullCoord` therefore covers the null-coordinate rule and says so
where those rows would be.

## Gates

* `RJdkVarHandleNullCoord`, new: 74 rows, byte-identical to HotSpot, and the
  before/after is the kill switch on ONE binary — 74 rows differ with
  `CRATONVM_VH_NULL_COORDINATE_NPE=0`, 0 with it on.
* regression suite **126/126, 0 failed, 0 harness-blindness flags**.
* the `VarHandle` bind engagement counters above, unchanged.

The matrix is GENERATED (`genprobe.py`'s output is checked in as the vector)
because each row is a signature-polymorphic call site whose exact static types
decide which access mode is invoked — a hand-typed table is where a stray cast
silently tests a different mode and the row passes for the wrong reason.

## Related

- `performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`
  (internal) — the work that found this, and the reference-read bind whose hot
  vector it came out of.
- `regression-suite/src/RJitVarHandleRefRead.java` — the sibling vector; its
  comment pointed at this page for the assertion it could not make, and that
  assertion now lives here instead.
