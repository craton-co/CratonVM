# `java.nio.StringCharBuffer.toString()` answers `""` when it is not called from bytecode

**Status:** OPEN, fully reproducible, narrowed to one triple. A targeted
workaround is in place for the callers that matter (see *Contained*), so no
shipped path is currently wrong — but the dispatch defect underneath is real and
`Method.invoke` still hits it.

## Reproducer

`probes/CharBufferUsersProbe`, and more narrowly:

```java
CharBuffer cb = CharBuffer.wrap("Hello, World");   // a java.nio.StringCharBuffer
cb.toString()                                  // "Hello, World"   correct
String.valueOf((Object) cb)                    // ""               WRONG
"" + cb                                        // ""               WRONG
new StringBuilder().append(cb).toString()      // ""               WRONG
CharBuffer.class.getMethod("toString").invoke(cb)  // ""            WRONG
```

The split is by **call route**, not by object: the same instance answers
correctly through a bytecode `invokevirtual` and empty through everything that
enters via `NativeContext::invoke_virtual` (`String.valueOf(Object)`,
`StringBuilder.append`, string concatenation) or through reflection.

A `java.nio.HeapCharBuffer` receiver — `CharBuffer.wrap(char[])`,
`CharBuffer.allocate(n)` — is correct on **every** route.

## What has been ruled out

Measured on `dev` (`6add9fc11`) and on the branch that contains it, real-JDK
mode, JDK 25:

* **Not the buffer's state.** Read through `Buffer`'s own fields, a
  `StringCharBuffer` from `wrap(s)` is byte-identical to HotSpot's:
  `mark=-1 position=0 limit=12 capacity=12 offset=0 isReadOnly=true`.
* **Not the bytecode chain.** `CharBuffer.toString()` is
  `return toString(position(), limit())`; `StringCharBuffer.toString(int,int)`
  is `str.subSequence(start+offset, end+offset).toString()`. Every link is
  correct in isolation on this VM: reflectively invoking the **abstract**
  `CharBuffer.toString(int,int)` against a StringCharBuffer returns
  `"Hello, World"`, and `invokeinterface CharSequence.subSequence(II)` /
  `CharSequence.toString()` on a `java.lang.String` receiver are correct
  (`probes/…` — `AbstractDispatchProbe`, `CsIfaceProbe` in the branch's
  scratch set).
* **Not abstract-override dispatch in general.** A user class with the same
  shape — an abstract base declaring a concrete `toString()` whose body calls an
  abstract method the subclass overrides — is correct on every route, including
  `String.valueOf`, `append` and `Method.invoke`.
* **Not the exception path.** No throw is raised or swallowed; the routes return
  an empty `java.lang.String`, not `null` and not `ClassName@hash`.
* **No native runs on the failing routes.** With
  `CRATONVM_DBG_CHARBUFFER=1` (added by this branch,
  `native-builtins/src/phases_late/charset_buffers.rs`) the direct call prints
  its trace and the failing routes print nothing at all; the
  `--dump-native-registry` invocation counter for
  `java/nio/StringCharBuffer.toString()Ljava/lang/String;` moves only for the
  direct calls.
* **Not `force_native_over_real_jdk_bytecode`.** Adding
  `java/nio/CharBuffer` / `java/nio/StringCharBuffer` `toString()` to that gate
  changed nothing, so this route does not consult it.

## The one structural difference left

`java/nio/StringCharBuffer` has a registered `toString()` native and
`java/nio/HeapCharBuffer` does not — and HeapCharBuffer is the shape that works
everywhere. So the suspicion is a resolution path that finds *a* registered
native for the triple, declines to run it, and then returns a default value
instead of falling through to the inherited bytecode. `admit_forced_native`'s
documented `Ok(None)` arm ("§7 step 3 sent a `Bridge` to the real bytecode") is
the shape to check first: the decline is correct, the *fallthrough* may not be.

The cheap next step is a trace inside `NativeContext::invoke_virtual`
(`vm/src/vm/vm_exec.rs`) for this triple, printing the resolved target and what
it returns. Everything above was derived without it and cannot narrow further.

## Contained

`invoke_to_string_opt` (`native-builtins/src/lang_string.rs`) now reads a
`java/nio/*CharBuffer*` receiver's text directly via
`charset_buffers::cb_read_text` instead of asking it, so `String.valueOf`,
string concatenation and `StringBuilder`/`StringBuffer.append` are correct.
`Method.invoke` on `CharBuffer.toString()` is still affected.

## A neighbour found the same way, NOT fixed

`AbstractStringBuilder.append(CharSequence s)` in the JDK appends the
sequence's **characters** (`append(s, 0, s.length())`, i.e. `charAt`-driven) for
anything that is not a `String` or an `AbstractStringBuilder` — it never calls
`toString()`. CratonVM's `native_sb_append_charsequence` calls `toString()`, so
a CharSequence whose `toString()` differs from its char content appends the
wrong text:

```java
CharSequence cs = new CharSequence() {
    public int length()             { return 12; }
    public char charAt(int i)       { return "Hello, World".charAt(i); }
    public CharSequence subSequence(int a, int b) { … }
    public String toString()        { return "CUSTOM"; }
};
new StringBuilder().append(cs)    // HotSpot: "Hello, World"   CratonVM: "CUSTOM"
```

Left alone deliberately: `append(CharSequence)` is a very hot path and the
JDK-faithful form pays a virtual `charAt` per character, so the change wants its
own measurement rather than being folded into a CharBuffer fix.

Found while closing
`charbuffer-wrap-string-subsequence-does-not-bounds-check-FIXED-20260806.md`.
That fix stops intercepting `CharBuffer.wrap(CharSequence)` in real-JDK mode,
so it returns the real `StringCharBuffer` HotSpot returns — which is what
brought this pre-existing defect onto the common path. Before it, only
`CharBuffer.wrap(s, start, end)` callers could reach it, and its record noted
the empty-encode half of the same problem for exactly that reason.
