# `--compatible`: `List.of(...)` is CratonVM's own list class, which a debugger shows

**Status: open, by design — filed 2026-09-27 by interpreter round i1 wave 25,
lane L1 (follow-up L1d), from the JDI conformance runner.**

## Evidence

`tools/jdi/run-jdi-conformance.sh`, scenario `L1W23JdiConformance`, modes
`compatible` and `compatible:nojit` only (`-` HotSpot 25, `+` CratonVM):

```
-   this.tags = instance of java.util.ImmutableCollections$List12
+   this.tags = instance of cratonvm.internal.UnmodifiableList
```

The field is `List<String> tags = List.of("x")`. Under `--compatible`, `List.of`
is served by CratonVM's synthetic collections, whose class is
`cratonvm/internal/UnmodifiableList`; the debugger reports the object's real
class (`ObjectReference.ReferenceType`). Under `--jdk-only` (the default) the
JDK's own `ImmutableCollections$List12` runs and the line matches HotSpot.

## What HotSpot does

Reports `java.util.ImmutableCollections$List12`, the JDK class.

## Why it stays

`--compatible` must not change (AGENTS.md: "`--compatible` mode must stay
byte-for-byte unchanged"), and its synthetic collections are its reason to
exist. Nothing in the JDWP server should disguise a class as another. The
runner's allow-list (`tools/jdi/known-differences.txt`) carries the two
lines for the two compatible modes, naming this page.

## What would make it go away

`--compatible` running the JDK's `ImmutableCollections` for `List.of` (the
jdk-only mode's behaviour), which is the direction the jdk-only work takes
(`docs/book/src/user-guide/jdk-only-mode.md`); then the allow-list entries go.
