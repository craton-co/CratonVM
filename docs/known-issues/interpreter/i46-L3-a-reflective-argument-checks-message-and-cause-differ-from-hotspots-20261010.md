# A reflective argument check's message and cause differ from HotSpot's

**Status: open (item 2; item 1 fixed in wave 46 by the lane that filed it)
— filed 2026-10-10 by interpreter round i1 wave 46, lane L3 (found while
measuring `tools/probes/interp/L3/L3W46ReflectiveChecks.java` for item 1 of
`docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`).
Both modes. Item 2 is a missing cause object; no control flow in the VM
depends on it.**

## Progress (wave 46) — lane L3

* **Item 1 fixed.** `lang_class::illegal_arg_exc_null_to_primitive` builds
  the cause with HotSpot's helpful message for the parameter's primitive
  (`Number.intValue()` for `int`, `boolean`, `char`; `longValue()`, ... for
  the others) and the `IllegalArgumentException` through its `(Throwable)`
  constructor, so its message is the cause's `toString()`. Probe rows
  `m-null-to-primitive-kind`, `m-null-to-long-kind`,
  `c-null-to-primitive-kind` of `L3W46ReflectiveChecks` (the base printed
  `java.lang.IllegalArgumentException: argument type mismatch`). Spring's
  `InvocableHandlerMethod.doInvoke` keys on the cause's class, which is
  unchanged.

## Evidence

HotSpot 25.0.3 (measured with a scratch program on the lane's machine;
the rows below are its `toString()`s):

1. **A null argument for a primitive parameter** (`Method.invoke` and
   `Constructor.newInstance`): `DirectMethodHandleAccessor.invoke:114` /
   `DirectConstructorHandleAccessor.newInstance:70` run
   `throw new IllegalArgumentException(e)` (read in
   `C:\craton\jdk25src\java.base\jdk\internal\reflect\DirectMethodHandleAccessor.java`),
   so the message is the cause's `toString()`:

   ```text
   java.lang.IllegalArgumentException: java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null
   ```

   (`longValue()` for a `long` parameter, `doubleValue()` for `double`,
   `intValue()` for `int`, `boolean` and `char`). CratonVM, read from the
   code: `lang_class::illegal_arg_exc_null_to_primitive` builds
   `IllegalArgumentException("argument type mismatch", new NullPointerException())`:
   message `argument type mismatch`, cause message null.
2. **A constructor argument of the wrong type**:
   `DirectConstructorHandleAccessor.newInstance:65` runs
   `throw new IllegalArgumentException("argument type mismatch", e)` with the
   `ClassCastException` as its cause
   (`class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')`,
   trace `ValueConversions.primitiveConversion:247`, `unboxInteger:81`, ...).
   `Method.invoke`'s twin arm (`invoke:108`) deliberately has NO cause ("No
   cause in IAE to be consistent with the old behavior"). CratonVM, read from
   the code: `coerce_arg_strict` returns a plain
   `RuntimeError::IllegalArgumentException`, so the constructor's
   `getCause()` is null.

## What would fix it

1. (Done in wave 46, see above.)
2. In `native_constructor_new_instance_body`'s coercion arm, for a reference
   argument that is not a wrapper of the primitive parameter, attach a
   `ClassCastException` built with HotSpot's cast message
   (`exceptions::hotspot_class_cast_message` builds that text for the
   interpreter's own casts) as the cause. Its own trace would need
   `ValueConversions.primitiveConversion` / `unbox<Wrapper>` frames, as the
   null case's cause got in wave 46 (`reflective_check_entries`).

A probe row per case (the exception's `toString()` and its cause's) should
go into `L3W46ReflectiveChecks` or a sibling when this is built.
