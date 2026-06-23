# HIB-CV-21 — `StringBuilder.repeat(int codePoint, int count)` unimplemented → real bytecode copies char[]→byte[] → `ArrayStoreException` (every timestamp literal)

**Severity:** High — **35 CV-only failing classes** (the single largest CV-only cluster in the full Hibernate suite). All date/time/temporal/JSON tests that format a timestamp literal.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/lang_string.rs`).
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

35 classes fail (HotSpot passes all) with:

```
java.lang.ArrayStoreException: arraycopy: incompatible array element types (src=Char, dest=Byte)
```

thrown with an empty Java stack trace (it originates in CratonVM's native `System.arraycopy`). Examples: `InstantTest`, `ExpressionsTest`, `JsonEmbeddableTest`, `MixedTimingEmbeddableGeneratorsTest`, `ProposedGeneratedTests`, `CreationUpdatedTimestampInEmbeddableDbTest`, `MySqlArrayOfTimestampsTest`, the JSON-embeddable family, …

## Root cause

Caller chain captured via a new `CRATONVM_DBG_ARRAYCOPY=1` diagnostic in `native_system_arraycopy`:

```
java/util/Arrays.copyOf
java/lang/AbstractStringBuilder.ensureCapacityNewCoder
java/lang/AbstractStringBuilder.repeat
java/lang/StringBuilder.repeat                                  <- (int codePoint, int count)
java/time/format/DateTimeFormatterBuilder$NumberPrinterParser.format   <- zero-padding
org/hibernate/type/descriptor/DateTimeUtils.appendAsTimestamp
org/hibernate/dialect/H2Dialect.appendDateTimeLiteral
```

`java.time.format.DateTimeFormatter` (JDK 21+) zero-pads numeric fields with `buf.repeat('0', n)` — the `StringBuilder.repeat(int codePoint, int count)` overload. CratonVM registered `repeat(CharSequence,int)` and `repeat(String,int)` natively, **but not `repeat(int,int)`**. So that call fell through to the real `java.lang.AbstractStringBuilder.repeat(int,int)` bytecode, which calls `ensureCapacityNewCoder` → `Arrays.copyOf(value, …)` → `System.arraycopy(value, 0, newValue, 0, n)`.

In a real JDK `AbstractStringBuilder.value` is a **compact-string `byte[]`**. CratonVM's `StringBuilder` backing is a **`char[]`**. So the real bytecode's `System.arraycopy` had `src = char[]` (CratonVM's `value`) and `dest = byte[]` (the freshly-allocated compact-string buffer) → `ArrayStoreException: src=Char, dest=Byte`.

This is the same class of CratonVM-`char[]`-StringBuilder vs real-JDK-`byte[]`-compact-string mismatch noted for the unintercepted `StringBuilder(CharSequence)` constructor — the fix is the same: intercept the method so the real byte[]-assuming bytecode never runs against CratonVM's char[] layout.

## Fix

Register `StringBuilder.repeat(II)Ljava/lang/StringBuilder;` (i.e. `repeat(int codePoint, int count)`) as a native that appends the code point `count` times into CratonVM's `char[]` representation (UTF-16, surrogate pair for supplementary planes), mirroring the existing `repeat(CharSequence,int)`/`repeat(String,int)` natives. The real `ensureCapacityNewCoder`/`Arrays.copyOf` byte[] path is then never reached.

A `CRATONVM_DBG_ARRAYCOPY=1` env-gated diagnostic (dumps the innermost Java caller frames + array identities on any element-type mismatch) was added to `native_system_arraycopy` to localize this; kept as a tool.

## Verification

5 representative classes that previously failed, all now green vs HotSpot:
`InstantTest` 2/2, `ExpressionsTest` 21/21, `JsonEmbeddableTest` 11/11,
`MixedTimingEmbeddableGeneratorsTest` 2/2, `ProposedGeneratedTests` 1/1 — **0** `ArrayStoreException`. Expected to clear all 35 classes in the cluster (single shared root cause).
