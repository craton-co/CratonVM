# Builder-layout reproducer: a redefinition corrupts every REAL StringBuilder

One `Mockito.mock(StringBuilder.class)` anywhere in a process used to change
what a *completely unrelated, real* `StringBuilder` does. Mockito is not part of
this reproducer — its inline mock maker was only ever a route to
`Instrumentation.redefineClasses`, and the corruption is the VM's.

`RedefineBuilderLayoutProbe` runs 28 builder operations on a fresh
`new StringBuilder("abcdefghij")`, redefines `AbstractStringBuilder` and
`StringBuilder` **with their own bytes**, and runs the same 28 again. Nothing
about the classes changes across the redefinition, so any difference is the VM's.

## Build and run

```bash
javac -d . cratonvm/Instrument.java RedefineBuilderLayoutProbe.java
cratonvm -cp . RedefineBuilderLayoutProbe   # must print PROBE PASS
java     -cp . RedefineBuilderLayoutProbe   # control: redefine skipped, PROBE PASS
```

`cratonvm/Instrument.java` is a declaration-only stand-in so
`Class.forName("cratonvm.Instrument")` resolves and binds to the VM's registered
native. Without it the probe degrades to a control run and says so. On HotSpot
no such native exists, so the redefine is skipped and the run is a control that
shows the measurement itself is stable.

## What it caught

CratonVM's builders are a synthetic two-field object — `char[] value`,
`int count` — not the JDK's compact `byte[] value` / `byte coder` / `int count`.
Every builder operation is therefore meant to resolve to a native shim, listed
in `is_string_builder_layout_native_override`
(`vm/src/runtime/interpreter/invoke.rs`). A redefinition evicts native shadows;
any operation *missing* from that list then falls back to the real JDK body,
which indexes a layout the object does not have.

Six were missing. Before the 2026-07-31 fix:

```
DIFF setLength(4)     before[len=4 str=abcd]        after[len=4 str=a]
DIFF setLength(0)     before[len=0 str=]            after[len=0 str=a]
DIFF setLength(len-1) before[len=9 str=abcdefghi]   after[len=9 str=a]
DIFF deleteCharAt(1)  before[len=9 str=acdefghij]   after[len=9 str=a]
DIFF replace(1,3,Q)   before[len=9 str=aQdefghij]   after[THREW ArrayStoreException:
                                arraycopy: incompatible array element types (src=Byte, dest=Char)]
DIFF ensureCapacity   before[len=11 str=abcdefghij!] after[len=11 str=           ]
DIFF trimToSize       before[len=11 str=abcdefghij!] after[len=11 str=           ]
DIFF repeat(x,3)      before[len=13 str=abcdefghijxxx] after[len=13 str=abcdefghij]
PROBE FAIL 8 operation(s) changed behaviour across the redefinition
```

Note `length()` stayed truthful while `toString()` did not — the two disagree,
which is why this presented as garbage rather than as an exception.

`ArrayStoreException: src=Byte, dest=Char` is the mechanism in one line: real
JDK bytecode copying out of a compact `byte[]` into the synthetic `char[]`.

## Why it mattered

javac's `JavaTokenizer` keeps one long-lived `StringBuilder` and calls
`sb.setLength(0)` at the top of every `readToken()`, plus
`sb.setLength(sb.length() - 1)` inside `scanOperator()`. With `setLength`
corrupting the buffer, every identifier javac lexed came out wrong, the parser
fell into `parseCompilationUnit`'s error-recovery loop, and Spring's
`AotIntegrationTests` chunk 4 — which compiles its generated sources with the
in-process javac after Mockito has mocked something — never finished. It looked
like a hang; it was a correctness bug two layers down. See
`spring-aot-cluster.md`.

## Related

[`../redefine-call-cost/`](../redefine-call-cost/) — the *throughput* half of
the same story: what a redefinition used to cost per subsequent call. That one
was fixed first and was, for a while, believed to be the whole of chunk 4. It
was not.
