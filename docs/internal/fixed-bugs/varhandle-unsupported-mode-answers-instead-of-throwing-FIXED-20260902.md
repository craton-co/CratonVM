# A `VarHandle` access mode the variable's type does not admit answers instead of throwing

## Status
**FIXED and CLOSED, 2026-09-02.** The second of the three rules the
null-coordinate work uncovered, and the one that page deferred by name:
"`getAndAdd*` / `getAndBitwise*` on a reference variable … needs its own oracle
sweep across every variable type, because the rule is not uniform".

It is not uniform, and the sweep is why this is a rule rather than a guess.

## The rule, measured

`RJdkVarHandleModeSupport` walks every arithmetic and bitwise access mode
against ten variable types — 160 rows against JDK 25:

| variable | `getAndAdd*` | `getAndBitwise*` |
|---|---|---|
| `boolean` | **UnsupportedOperationException** | ok |
| `byte` `char` `short` `int` `long` | ok | ok |
| `float` `double` | ok | **UnsupportedOperationException** |
| any reference | **UnsupportedOperationException** | **UnsupportedOperationException** |

**Arithmetic is the NUMERIC primitives; bitwise is the INTEGRAL ones plus
`boolean`.** The two sets differ at both ends, and `boolean` against `float` is
the pair that catches anyone who reasons "primitives yes, references no". The
`Acquire`/`Release` variants follow their base mode exactly — checked, not
assumed, for `boolean`, `int`, `float` and a reference.

## The order, also measured

`UnsupportedOperationException` **beats** `NullPointerException`. Swept across
all ten types: where the mode is unsupported a null receiver still yields UOE;
where it is supported the null receiver yields NPE. Uniform, so the guard runs
before `vh_check_leading_coordinate` and that is the whole interaction between
the two rules.

## What CratonVM did

Answered, on every row HotSpot refuses. Of the sweep's original 166 rows, 57
raise UOE on HotSpot and CratonVM returned a value for all 57 — `null` 28
times, a `float` 20 times (a bitwise operation on a `float` variable), a
`boolean` 5 times, and two silent write successes.

## The fix

`vh_check_access_mode_supported` in `native-builtins/src/lang_invoke.rs`, called
from the two registered families (`varhandle_get_and_add`,
`varhandle_get_and_bitwise`) ahead of the null-coordinate guard.

Cost is one `Arc` deref and one byte compare: the variable's descriptor is on
the metadata every access mode already consults, and its FIRST byte is the whole
test. The read, write and CAS families do not call it at all.

`CRATONVM_VH_UNSUPPORTED_MODE_UOE=0` restores the old behaviour. It is separate
from `CRATONVM_VH_NULL_COORDINATE_NPE` on purpose: the two rules interact, so a
single switch could not isolate either.

## Gates

* `RJdkVarHandleModeSupport`, new: 160 rows, **byte-identical to HotSpot**, and
  the before/after is the kill switch on ONE binary — **53 rows differ** with
  `CRATONVM_VH_UNSUPPORTED_MODE_UOE=0`, 0 with it on.
* `RJdkVarHandleNullCoord` unchanged and still byte-identical, which is what
  says the new guard did not disturb the rule that ships beside it.
* regression suite **128/128, 0 failed, 0 harness-blindness flags**.

### A vector that proved nothing, for one revision

The first verification of this vector reported **0 differences in both arms** —
including the arm where the fix was switched off, which cannot be right. The
cause: a `char` variable's modes return values like U+0001, and one raw control
byte makes `diff` classify the file as BINARY and print `Binary files differ`
instead of line differences, so `grep -c '^<'` counted nothing. Two files
differing on 53 rows compared clean.

The vector now renders every value as pure ASCII (`<U+0001>`), which is what a
cross-VM TEXT diff is owed. Worth keeping in mind for any vector that prints a
value it did not choose: **check the comparison can fail before trusting that it
passed.**

## Still open, from the same sweep

A **read-only (final-field) handle**: HotSpot refuses every write mode,
CratonVM performs the write. Different rule, different route — it is a property
of the HANDLE rather than of the variable's type, and needs field-level access
flags to reach a native. Filed as
[`../../known-issues/jdk-only/varhandle-final-field-handle-performs-the-write-20260902.md`](../../known-issues/jdk-only/varhandle-final-field-handle-performs-the-write-20260902.md).

## Related

- `fixed-bugs/varhandle-null-coordinate-answers-instead-of-throwing-FIXED-20260902.md`
  — the first of the three, and the work whose hot vector started all of it.
- `regression-suite/src/RJdkVarHandleModeSupport.gen.py` — the generator. The
  vector is regenerated, never hand-edited: each row is a signature-polymorphic
  call site whose exact static types decide which access mode runs.
