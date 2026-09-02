# A `VarHandle` over a FINAL field performs the write instead of refusing it

## Status
**OPEN, opened 2026-09-02.** Found by the sweep that closed the access-mode
support rule (`fixed-bugs/varhandle-unsupported-mode-answers-instead-of-throwing-FIXED-20260902.md`,
internal). Filed rather than folded in, because it is a different rule reached
by a different route and the fix is plumbing rather than a check.

## Symptom

`MethodHandles.Lookup.findVarHandle` on a **final** field yields a handle whose
WRITE modes are unsupported. HotSpot refuses every one of them; CratonVM
performs the write.

```java
static class H { final int fin = 9; }
VarHandle VH = lookup().findVarHandle(H.class, "fin", int.class);

VH.set(h, 5);                 // HotSpot: UnsupportedOperationException   CratonVM: writes 5
VH.setVolatile(h, 5);         // HotSpot: UnsupportedOperationException   CratonVM: writes 5
VH.getAndSet(h, 5);           // HotSpot: UnsupportedOperationException   CratonVM: returns 5
VH.compareAndSet(h, 0, 5);    // HotSpot: UnsupportedOperationException   CratonVM: returns false
VH.getAndAdd(h, 5);           // HotSpot: UnsupportedOperationException   CratonVM: returns 5
```

Measured on JDK 25 against CratonVM at `120bb7c37`, five rows, all five
divergent.

## Severity
**LOW-MEDIUM.** No workload in the corpus is known to reach it, because
well-typed code does not write a final field through a handle it asked for by
name. What makes it worth a page is the species: a **silent successful write to
a field the language guarantees is immutable**. Anything caching a final
field's value — the JIT's own constant folding included — is entitled to assume
it cannot change, so this is a correctness hazard with a wide blast radius the
day something does reach it, not merely a wrong exception.

## Why it was not fixed with the mode-support rule

The two look like one rule because they raise the same exception, and they are
not.

* **Mode support by variable TYPE** — `getAndAdd*` on a `boolean`,
  `getAndBitwise*` on a `float`, either on a reference — is answerable from the
  handle's own metadata: `VarHandleMeta::field_desc` is already loaded on every
  access, and the check is one byte compare. That one is fixed.
* **Read-only-ness is a property of the HANDLE**, decided at
  `findVarHandle` time by the field's `ACC_FINAL` flag. `VarHandleMeta` does not
  record it, and nothing in `NativeContext` exposes field-level access flags —
  `Class::is_final` is about the class. So closing this needs a new field on the
  meta, a way for the creating native to learn the flag, and a decision about
  where in the order it sits.

## The order, already measured

HotSpot's precedence, swept across all ten variable types in
`RJdkVarHandleModeSupport`: **unsupported-mode beats null-coordinate**. Where a
mode is unsupported, a null receiver still yields
`UnsupportedOperationException`; where it is supported, the null receiver yields
`NullPointerException`.

A read-only refusal would have to be placed against those two. Not measured
here, and worth measuring before implementing: `VH.set((H) null, 5)` on a
final-field handle asks which of the three wins.

## Next step

1. Record finality on `VarHandleMeta` at `findVarHandle`/`findStaticVarHandle`
   time. That is where the field is already being resolved, so the flag is in
   hand — the work is getting it out of the class manager and into the native.
2. Sweep the three-way order (read-only vs unsupported-mode vs null coordinate)
   before choosing where the check goes.
3. Extend `RJdkVarHandleModeSupport.gen.py`, which already generates a
   final-field section and currently omits it with a comment pointing here.
4. Land behind a kill switch, like its two siblings
   (`CRATONVM_VH_NULL_COORDINATE_NPE`, `CRATONVM_VH_UNSUPPORTED_MODE_UOE`), and
   run the suites: this one CAN change a passing workload, because code that
   writes a final field today succeeds today.

## Repro

```bash
javac -d <out> RJdkVarHandleModeSupport.java   # re-add the final-field section first
java     -cp <out> RJdkVarHandleModeSupport | grep final-int   # five UnsupportedOperationException
cratonvm --java-home <jdk> -cp <out> RJdkVarHandleModeSupport | grep final-int
```

## Related

- `fixed-bugs/varhandle-unsupported-mode-answers-instead-of-throwing-FIXED-20260902.md`
  (internal) — the sweep that found this, and the two rules that are fixed.
- `fixed-bugs/varhandle-null-coordinate-answers-instead-of-throwing-FIXED-20260902.md`
  (internal) — the first of the three, and the one whose hot vector started all
  of it.
