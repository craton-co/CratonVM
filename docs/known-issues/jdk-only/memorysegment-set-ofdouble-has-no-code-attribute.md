# `MemorySegment.set(ValueLayout$OfDouble, long, double)` has no Code attribute

**Status:** OPEN, filed 2026-08-10. Found while verifying the FFM half of the
retired object-layout record (`fixed-bugs/jdk-only-fabricated-object-layouts-FIXED-20260810.md`
step 3), which is why it is filed separately: it is **not** a layout defect. The
layouts are right; the method to write through them is missing.

## What happens

```java
try (Arena arena = Arena.ofConfined()) {
    MemorySegment seg = arena.allocate(64);
    seg.set(ValueLayout.JAVA_INT, 0, 0x0BADF00D);   // ok
    seg.set(ValueLayout.JAVA_LONG, 8, -1L);         // ok
    seg.set(ValueLayout.JAVA_DOUBLE, 16, 1.5);      // AbstractMethodError
}
```

```
java.lang.AbstractMethodError: method
java/lang/foreign/MemorySegment.set(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V
has no Code attribute
```

`probes/W2ValueLayoutProbe` is the reproducer; its `segment()` section fails on
that one line and passes the `JAVA_INT`, `JAVA_LONG` and `JAVA_BYTE` stores
above it, so the shape is one missing overload rather than a broken segment.

## What it is not

* **Not a layout defect, and not new.** Identical on the pre-fix binary and in
  both `--real-jdk` and `--jdk-only`; every one of the thirteen layout
  properties the probe prints (`byteSize`, `byteAlignment`, `order`, `carrier`
  for ten constants plus `ADDRESS`/`ADDRESS_UNALIGNED`) matches Temurin 25.0.3
  exactly. The layout objects are fine.
* **Not the byte-order defect fixed the same day.** That one made every constant
  report `BIG_ENDIAN` and is closed; see the retired record's step 3.

`MemorySegment` is an interface, so "no Code attribute" means dispatch reached
the interface method itself rather than an implementation — the same family as
`bug-interface-method-dispatch-no-code-attribute` in the comparison handoff, not
a missing native.

## How to reproduce

```sh
cratonvm --real-jdk --java-home "$JAVA_HOME" -cp . W2ValueLayoutProbe
```

The transcript should be byte-identical to `java -cp . W2ValueLayoutProbe`. It
differs on exactly two lines today, both from this one store.

## What to check first

Which `set` overloads resolve. The `int`/`long`/`byte` stores in the same
`try`-with-resources succeed, so whatever selects the implementation is
answering for those carriers and not for `double` — start by comparing how the
four differ at the dispatch site rather than by looking for a missing method.
