# `java.nio.StringCharBuffer.toString()` answered `""` off bytecode — a duplicate registration, not a dispatch defect

| | |
|---|---|
| **Status** | ✅ **FIXED** — 2026-08-07. Both defects the doc named, plus the workaround it installed, are gone. |
| **Root cause** | `java/nio/CharBuffer.toString()Ljava/lang/String;` was registered **twice**; the later, `hb`-only registration won and returned `""` for any CharBuffer with no backing `char[]`. |
| **Scope** | `native-builtins/src/phases_late/charset_buffers.rs`, `native-builtins/src/lang_string.rs`, `native-builtins/tests/duplicate_registration_gate.rs`. |
| **Repro** | `probes/ScbRouteProbe`-shaped matrix below; `probes/CharBufferUsersProbe` and `probes/CharBufferWrapProbe` as the regression差. |

## What it was

```java
CharBuffer cb = CharBuffer.wrap("Hello, World");   // a java.nio.StringCharBuffer
cb.toString()                                       // "Hello, World"   correct
CharBuffer.class.getMethod("toString").invoke(cb)   // ""               WRONG
```

`register_p62_char_buffer` registered `java/nio/CharBuffer.toString()` at
`charset_buffers.rs:1373` — delegating to `cb_to_string_range`, which reads a
`StringCharBuffer`'s wrapped sequence out of `str` — and then registered the
**same triple again** ~340 lines later at `:1717`, reading only the backing
array through `cb_read_hb` and returning `""` when there was none.
`NativeMethodRegistry::register` is last-write-wins, so `:1717` owned the slot.
A real `StringCharBuffer` has no `hb`.

**The split was by which class the route resolved against, and that is what
made it look like a dispatch bug.** A bytecode `cb.toString()` resolves the
EXACT class and finds `java/nio/StringCharBuffer.toString()` — a separate,
correct registration — so it was right. `Method.invoke` and
`NativeContext::invoke_virtual` both land in `invoke_on_class_shared`, which
resolves the inherited declaration on `java/nio/CharBuffer`, so they got the
shadowing twin and its empty string.

The original record ruled out buffer state, the bytecode chain,
abstract-override dispatch in general, the exception path and
`force_native_over_real_jdk_bytecode` before concluding "a resolution path that
finds a registered native, declines to run it, and returns a default". The
native was not declined. A *different* native ran.

### The rule-out that pointed the wrong way

> *"No native runs on the failing routes … the `--dump-native-registry`
> invocation counter for `java/nio/StringCharBuffer.toString()` moves only for
> the direct calls."*

That reading is correct and the inference from it is not.
`--dump-native-registry`'s `invocations` is bumped by
`NativeMethodRegistry::record_invocation`, which takes a `NativeMethodId`. The
dispatch inside `invoke_on_class_shared` looks the callback up with
`native_methods.find(...)` — which returns a bare `NativeCallback`, no id — and
calls it through `safe_native_call`. **That path records nothing.** So a zero on
this route means "not counted", not "not run", and the counter for the
*correct* triple staying at zero was consistent with a *different* triple
running all along.

## The neighbour, also fixed

`AbstractStringBuilder.append(CharSequence s)` appends the sequence's
**characters** (`append(s, 0, s.length())`, `charAt`-driven) for anything that
is not a `String` or an `AbstractStringBuilder` — it never calls `toString()`.
`native_sb_append_charsequence` and `native_sb_append_charsequence_off_len`
called `toString()`, so a `CharSequence` whose `toString()` disagrees with its
characters appended the wrong text:

```java
CharSequence cs = /* charAt yields "Hello, World", toString() returns "CUSTOM" */;
new StringBuilder().append(cs)      // HotSpot: "Hello, World"   was: "CUSTOM"
String.valueOf((Object) cs)         // HotSpot: "CUSTOM"         (unchanged — valueOf DOES call toString)
```

The record left this alone deliberately, on the grounds that `append(CharSequence)`
is hot and the JDK-faithful form pays a virtual `charAt` per character. The fix
keeps both properties: `charsequence_chars` reads the text in Rust for the
three shapes whose `charAt` is the JDK's own and provably agrees with
`toString()` — `java.lang.String`, `StringBuilder`/`StringBuffer`, and the
`java.nio` CharBuffer family (package-private constructors, so nothing outside
`java.nio` can override `charAt`) — and walks `charAt` for everything else,
which is exactly what HotSpot does. The hot paths stay native; only the case
that was wrong changed cost.

`append(CharSequence, int, int)` had the same defect and is fixed the same way.
Its deliberate clamping (rather than the JDK's `IndexOutOfBoundsException`) is
preserved, with the original justification carried forward.

## The workaround is deleted, not kept

`invoke_to_string_opt` had a `java/nio/*CharBuffer*` branch that read the
buffer's text instead of asking it, so `String.valueOf`, string concatenation
and `StringBuilder.append` were correct while the underlying defect stood. That
branch is **removed**. Keeping it would have left the repaired route
unexercised by every caller that matters, which is how the next regression
there goes unnoticed. With it gone, every route is correct on its own.

## Verification

The full route matrix, both concrete buffer shapes, against a HotSpot 25
oracle — `diff` is empty:

| route | `StringCharBuffer` before | after | HotSpot |
|---|---|---|---|
| `cb.toString()` (bytecode) | `Hello, World` | `Hello, World` | `Hello, World` |
| `String.valueOf((Object) cb)` | `Hello, World` (workaround) | `Hello, World` | `Hello, World` |
| `"" + cb` | `Hello, World` (workaround) | `Hello, World` | `Hello, World` |
| `new StringBuilder().append(cb)` | `Hello, World` (workaround) | `Hello, World` | `Hello, World` |
| `new StringBuffer().append(cb)` | `Hello, World` (workaround) | `Hello, World` | `Hello, World` |
| `Method.invoke(CharBuffer.toString)` | **`""`** | `Hello, World` | `Hello, World` |
| `Method.invoke(concrete toString)` | **`""`** | `Hello, World` | `Hello, World` |
| `charAt` walk | `Hello, World` | `Hello, World` | `Hello, World` |
| `append(custom CharSequence)` | **`CUSTOM`** | `Hello, World` | `Hello, World` |
| `append(custom CS, 0, len)` | **`CUSTOM`** | `Hello, World` | `Hello, World` |
| `String.valueOf(custom CS)` | `CUSTOM` | `CUSTOM` | `CUSTOM` |

`HeapCharBuffer` (`wrap(char[])`, `allocate`) was correct on every route before
and after, which is what made the two shapes differ: it has no exact-class
`toString()` registration to be right about.

* `probes/CharBufferUsersProbe` and `probes/CharBufferWrapProbe`: byte-identical
  to HotSpot, with the workaround removed.
* `org.apache.jasper.compiler.TestEncodingDetector` (a real JSP-compiling
  Tomcat class, the kind that reaches `CharBuffer.wrap` through Jasper):
  `OK (22 tests)`.
* `cratonvm-native-builtins` 3335 passed / 0 failed. `cratonvm-vm` 2450 passed /
  1 failed — `memory::addr_keyed::tests::the_address_keyed_table_census_is_complete`,
  which fails identically on a pristine `origin/dev` tree (it names
  `native-builtins/src/net_phase_e.rs`, untouched here).

## Regression test

`native-builtins/tests/duplicate_registration_gate.rs::char_buffer_to_string_is_not_shadowed`
asserts this one triple has no shadowed registration, and names both provenance
sites when it does.

The file's existing whole-workspace ratchet already counted this row — deleting
the duplicate moved it from **1207 to 1206** — but that number cannot fail for
one triple coming back, and its frozen baseline is `0`, so the gate is red for
1206 other reasons and would not have flagged a reintroduction. Negative control
(duplicate restored, test kept):

```
char_buffer_to_string_is_not_shadowed ... FAILED
  lost at native-builtins/src/phases_late/charset_buffers.rs:1373 [bridge]
    -> WINS at native-builtins/src/phases_late/charset_buffers.rs:1717 [bridge]
```

That the evidence was sitting in a frozen census nobody reads is its own
finding: `cargo test --workspace` has not run in CI for as long as the tree has
been unformatted, because `cargo fmt --all --check` is the first step of the
same job.
