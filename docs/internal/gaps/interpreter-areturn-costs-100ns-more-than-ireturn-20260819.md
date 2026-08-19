# `areturn` costs ~100ns more than `ireturn`, and it took three probe rewrites to say that honestly — 2026-08-19

**Status:** MEASURED, NOT FIXED. The probe is committed; the fix is scoped
below and deliberately not attempted here.

**Instrument:** `probes/AreturnCostProbe.java`.

## The measurement

All four arms are opcode-for-opcode identical apart from the return kind
(verified with `javap`, census below). Three passes, `--nojit`:

| | HotSpot `-Xint` | CratonVM |
|---|---|---|
| gap, short descriptor (~1 char scan) | **+0.2** | **+69.7 / +101.0 / +141.0** |
| gap, LONG descriptor (~145 char scan) | **-1.0** | **+176.6 / +190.9 / +179.9** |

HotSpot has no reference-return penalty at either descriptor length. CratonVM
has **~100ns fixed, plus ~79ns for 144 extra descriptor characters** — about
0.55 ns per character scanned.

Two components, needing different fixes:

1. **The descriptor scan.** `crate::jit::return_type(frame.method_descriptor())`
   walks the descriptor to `)` on every reference return, recovering a byte that
   is a property of the method. The same re-derivation shape this audit has
   fixed six times elsewhere.
2. **A fixed ~100ns.** The `areturn` arm routes the value through
   `cv.to_value()` -> `coerce_value_for_return_validated` ->
   `push_unchecked(value)`, where i/l/f/d-return copy the raw `CompactValue` via
   `push_compact(cv)`. That round trip was REMOVED for those four in an earlier
   fix and deliberately left on `areturn` "to normalize jobject-as-Long
   handles" — a normalization that returns the value unchanged whenever it is
   already a reference, which is essentially always.

## Three probe rewrites, and why the first answer was wrong twice over

The first version reported a ~90ns gap FLAT with descriptor length — which would
have acquitted the scan and aimed any fix at the round trip alone. Both halves
were wrong, and each came from the arms differing in something other than the
return:

1. **The consumer.** `a += iShort()` (iadd) against `if (aShort() != null)`
   (ifnonnull). Both arms now consume with a one-operand branch: `ifeq` against
   `ifnull`.
2. **The callee body.** `iShort() { return 1; }` (iconst_1) against
   `aShort() { return SINK; }` — a **getstatic**, which costs ~161ns alone in
   this interpreter and accounted for most of the reported gap. Both callees are
   now parameter passthrough: `iload_0/ireturn` against `aload_0/areturn`.
3. **The caller's argument.** Fixing (2) by passing `SINK` re-introduced the
   same getstatic in the CALLER. Both arguments now come from a local hoisted
   out of the loop: `iload_1` against `aload_1`.

Final opcode census per kernel, which is what "identical" has to mean:

```text
  kIShort   16 invokestatic   16 iload_1   16 ifeq     17 iinc
  kAShort   16 invokestatic   16 aload_1   16 ifnull   17 iinc
  kILong   112 aload_2  16 invokestatic  16 iload_1  16 ifeq   17 iinc
  kALong   112 aload_2  16 invokestatic  16 aload_1  16 ifnull 17 iinc
```

**This tree has recorded the same lesson before** — a checkcast measurement once
inflated ~180ns by reading a field through the cast. When two arms differ in
more than the thing under test, the difference is attributed to whatever you
were looking for. Check the bytecode, not the source. It took three iterations
here because each fix introduced the next confound.

## Why the fix is not attempted here

**The scan half** wants the return-type byte cached per METHOD, and the natural
home is `CachedBytecodeMethod` — which its own `method_index` doc records as
unable to take a new field without touching 38 struct literals across four
crates, none with a `..` tail. Caching per-FRAME buys nothing: a method executes
exactly one return per invocation, so a per-frame cache pays the same scan it
saves.

**The fixed half** wants `areturn` to copy the raw `CompactValue` when the slot
is already a reference, exactly as i/l/f/d-return do. The obvious gate,
`CompactValue::is_object()`, is documented as "the weakest of the decoders": it
passes a primitive long whose bits collide into `SUB_OBJECT` — precisely the BC
safegcd `0xFFFC_...` accumulator case the current round trip exists to
normalize. A wrong gate is SILENT: a collided long would return as a plausible
object reference. That needs the equivalence-test discipline the argument-tag
scan got, not a tail-end edit.

## What is NOT claimed

The ~100ns fixed component is measured but not ATTRIBUTED. `to_value()`'s
SUB_OBJECT path applies only a context-free plausibility filter (no heap
access), and `coerce_value_for_return_validated` returns the value unchanged for
`Value::Object(_)`, so neither obviously accounts for ~300 cycles. Pinning it
needs sub-phase instrumentation inside the arm, which this record does not have.
Naming the round trip as the LOCATION is justified; naming any one call inside
it as the COST is not.
