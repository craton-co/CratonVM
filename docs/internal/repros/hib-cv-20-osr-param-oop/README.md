# HIB-CV-20 regression repro — OSR oop-parameter stale across moving GC

Hibernate-free reproducers for the JIT OSR hang fixed in
`docs/internal/jit-osr-param-oop-stale-register-FIXED.md`.

## `OsrFillGc2.java` (primary)

A hot `Arrays.fill(byte[], 0, len, val)` on a **freshly-allocated young** `byte[]`,
under young-GC pressure. The fill loop goes hot → OSR-compiles; the array param
lives in a callee-saved register across `Arrays.rangeCheck`'s safepoint. Pre-fix, a
moving young GC during that call left the register stale → out-of-bounds writes →
hang/corruption. HotSpot completes; pre-fix CratonVM hung at `r=0`.

```
javac OsrFillGc2.java
# pre-fix: hangs at @@PROG r=0; post-fix: @@DONE
cratonvm.exe --java-home <jdk25> -Xmx48m -cp . OsrFillGc2 8000
```

## `OsrFillGc.java` (control)

Same shape but the filled `byte[]` is a **static** (old-gen, non-moving) field.
It does NOT corrupt even pre-fix — demonstrating the bug requires a *moving*
(young/evacuated) array, which is why a static buffer masks it.

## Real repro

`apps/hibernate-orm/cratonvm-bug-reports/run-20260622/MinSeq.java` — parse 13 ORM
XSDs via Xerces `SchemaFactory.newSchema()` in one process. Pre-fix: hangs ~5th–6th
parse under default JIT; post-fix: `@@ALLDONE`.
