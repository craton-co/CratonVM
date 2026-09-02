# A `VarHandle` over a FINAL field performs the write instead of refusing it

## Status
**FIXED and CLOSED, 2026-09-02.** The third and last of the `VarHandle`
conformance rules the null-coordinate work uncovered, and the only one of the
three that can change a workload which passes today — code that writes a final
field succeeded before this.

## What it was

`MethodHandles.Lookup.findVarHandle` on a **final** field yields a handle whose
WRITE modes are all unsupported. HotSpot refuses every one; CratonVM performed
the write.

```java
static class H { final int fin = 9; }
VarHandle VH = lookup().findVarHandle(H.class, "fin", int.class);
VH.set(h, 5);          // HotSpot: UnsupportedOperationException   CratonVM: wrote 5
```

**A silent successful write to a field the language guarantees is immutable.**
Anything that caches a final field's value — constant folding in the JIT
included — is entitled to assume it cannot change, which is what made this
worth closing even though no corpus workload reaches it.

`RJdkVarHandleModeSupport` now covers the whole surface: all four `set` modes,
the five CAS modes, three compare-and-exchange, `getAndSet*`, `getAndAdd*` and
the nine `getAndBitwise*`, on a final `int`, a final reference and a
`static final` — **42 rows**, every one of which CratonVM answered and now
refuses.

The four READ modes are the control and still answer: a read-only handle is
read-only, not dead.

## The order, measured

This page opened naming the order as its one unmeasured question. Swept:

```text
VH_FIN.set((H) null, 5)  ->  UnsupportedOperationException, not NullPointerException
```

**Read-only beats the null coordinate.** Read-only against an unsupported MODE
is not observable — both raise `UnsupportedOperationException` — so that pair's
order is free. The full precedence, and the order the three guards run in:

1. read-only handle (this rule) — `UnsupportedOperationException`
2. access mode vs variable type — `UnsupportedOperationException`
3. null coordinate — `NullPointerException`

## The fix, and the assumption that turned out wrong

This page said closing it needed "field-level access flags reaching a native,
which is cross-crate plumbing rather than a check". **That was wrong, and
checking it took one grep.** `FieldMetadata` has carried `access_flags` all
along and `NativeContext::declared_fields` is already available to natives —
`lookup_require_field`, forty lines above the site that needed it in the same
file, already walks exactly that. What looked like plumbing was one
`access_flags & ACC_FINAL`.

So the shape is:

* `VarHandleMeta` gains `read_only`, decided ONCE at `findVarHandle` /
  `findStaticVarHandle` time, where the field is already being resolved. Not
  derived per access: that would mean a `declared_fields` walk — a
  `Vec<FieldMetadata>` of owned `String`s — on every `set`, and `set` is
  698 000 calls on one probe.
* `vh_check_read_only` runs first in the six WRITE entry points. `varhandle_get`
  does not call it.
* Marking is a separate `vh_mark_read_only` rather than a parameter on
  `alloc_instance_var_handle`, because that helper also mints array-element and
  byte-view handles, whose variables are never final — a parameter would put
  the question to callers that cannot answer it.

The static path resolves the class id only to ask the finality question, and
only if the class is already loaded; a miss leaves the handle writable, which
is the previous behaviour rather than a guess.

`CRATONVM_VH_READ_ONLY_HANDLE_UOE=0` reverts. Third switch, not a shared one,
for the reason the other two are separate: the rules interact, and a shared
switch could isolate none of them.

## Gates

* `RJdkVarHandleModeSupport`, extended to **202 rows**, byte-identical to
  HotSpot — and each switch reverts its OWN rule and no other:
  `CRATONVM_VH_READ_ONLY_HANDLE_UOE=0` moves **42** rows,
  `CRATONVM_VH_UNSUPPORTED_MODE_UOE=0` moves **53**.
* `RJdkVarHandleNullCoord` unchanged and still byte-identical.
* regression suite **128/128, 0 failed, 0 harness-blindness flags**.

## The three rules, together

| # | rule | raises | vector |
|---|---|---|---|
| 1 | null coordinate | `NullPointerException` | `RJdkVarHandleNullCoord` (74 rows) |
| 2 | access mode vs variable type | `UnsupportedOperationException` | `RJdkVarHandleModeSupport` |
| 3 | read-only (final-field) handle | `UnsupportedOperationException` | `RJdkVarHandleModeSupport` |

All three began as one line in a hot vector written for an unrelated perf bind:
`String bogus = (String) intVarHandle.get(h)`. The first page named three broken
shapes; sweeping the axis found 75, and sweeping the next two axes found 57 and
42 more.

## Related

- `fixed-bugs/varhandle-null-coordinate-answers-instead-of-throwing-FIXED-20260902.md`
- `fixed-bugs/varhandle-unsupported-mode-answers-instead-of-throwing-FIXED-20260902.md`
- `regression-suite/src/RJdkVarHandleModeSupport.gen.py` — the generator. Both
  vectors are regenerated, never hand-edited.
