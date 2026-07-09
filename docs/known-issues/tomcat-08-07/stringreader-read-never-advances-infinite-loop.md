# `StringReader.read()` never advances position → infinite loop in any `BufferedReader`/`StringReader` parse loop

**Status:** OPEN — root-caused; NOT fixed (same "phantom native" dispatch
mystery as the sibling `ByteBuffer.address` bug — see that doc for the
shared pattern). **Severity: HIGH** — any code that reads a `String` via
`new BufferedReader(new StringReader(s))` (or a bare `StringReader`)
character-by-character in a `while ((c = read()) != -1)`-style loop hangs
forever instead of terminating at EOF. This is an extremely common JDK
idiom (config/rule-file parsing, `Properties`-like text parsers, etc.).
**HotSpot:** unaffected (n/a — CratonVM-only).

Found 2026-07-09 investigating why `TestRewriteValve` (isolated re-run of
[`accesslogvalve-rewritevalve-connection-failures.md`](accesslogvalve-rewritevalve-connection-failures.md))
hangs completely instead of reproducing its originally-reported `-1`/`400`
failure.

## Symptom

```java
StringReader sr = new StringReader("hello");
int n;
while ((n = sr.read()) != -1) { System.out.println((char) n); }
```
On CratonVM this never terminates — every call to `read()` returns `'h'`
(104) again. Confirmed both through a bare `StringReader` and through a
`BufferedReader` wrapping one (same result — `BufferedReader`'s own
buffering logic is not implicated, the bug is in `StringReader.read()`
itself).

Tomcat's `RewriteValve.parse(BufferedReader)` (which reads the `.rewrite`
rules config character-by-character) hits this on `TestRewriteValve`'s
in-memory rule-file fixture and spins forever — a `--stack-dump-on-timeout
30` capture showed the main thread stuck with tens of millions of
back-to-back
`org/apache/catalina/valves/rewrite/RewriteValve.parse(...)V ->NATIVE
java/io/StringReader.read([CII)I` entries in the native-call ring buffer,
zero forward progress, well past a 30s watchdog deadline.

## Root cause (logic bug, confirmed but not proven to be live — see below)

The registered `java/io/StringReader.read()V` native in
`native-builtins/src/phases_early.rs` (~line 3138) computes the "advance the
reader" step incorrectly:

```rust
let ch = source.chars().next().unwrap_or('\0') as i32;
let rest = ctx.create_string(&source[ch.min(source.len() as i32) as usize..]);
```

This uses the **character's own code-point value** (`ch`, e.g. `104` for
`'h'`) as a **byte index** into the remaining source string, clamped by
`.min(source.len())` — instead of advancing by exactly one character's
UTF-8 byte width (`ch.len_utf8()`, normally `1` for ASCII). For any string
shorter than the read character's code point (virtually all real text —
`'h'` = 104 already exceeds most short strings), this always clamps to
`source.len()`, i.e. it would truncate the *whole remaining string* to
empty on the *first* read rather than stepping forward by one character —
the opposite direction of bug from what's observed (the observed bug is
"never advances," not "advances too far").

**This means the logic bug above, even though real and worth fixing, does
not by itself explain the observed symptom** — if it were the live
implementation, a second `read()` call would see an empty string and
correctly return `-1`, not `'h'` again. The actual live behavior points to
the position/updated-string write not being visible to the next call at
all, which is consistent with this registration **not being the code path
that actually runs** (see next section).

## Investigation dead-ends (same pattern as the ByteBuffer bug — read first)

Exactly like `ByteBuffer.allocate()`
(see [`bytebuffer-address-unset-aioobe.md`](bytebuffer-address-unset-aioobe.md)),
`classloading/src/class_manager.rs` (~line 10985) unconditionally injects
`java/io/StringReader`'s entire native surface (`<init>`, both `read`
overloads, `ready`, `close`, `skip`, `reset`, `markSupported`) as
`MethodAccessFlags::NATIVE` with no `Code` attribute — so there is no real
bytecode to ever fall back to, for either JDK mode; this method **must**
resolve through native dispatch.

Two candidate implementations exist, and — via the same unconditional
`eprintln!`-in-closure technique used for the `ByteBuffer` bug — **neither
is on the live path**:

1. **`native-builtins/src/phases_early.rs`** (`register_phase51_natives`,
   unconditionally called, not feature-gated, category not explicitly
   `SyntheticStub`) — the buggy index-arithmetic version quoted above. An
   unconditional debug `eprintln!` placed directly in this closure produced
   **zero output** across multiple rebuild+rerun cycles.
2. **`native-io/src/lib.rs::register_string_rw_natives`** — has the
   *correctly-shaped* 3-field synthetic layout (`content`/`pos`/`length`,
   documented in `classloading/src/class_manager.rs`'s own field-layout
   table) and is explicitly registered under `NativeKind::SyntheticStub`
   with a comment explaining the intent ("fake-JDK StringReader links...
   real JDK bytecode still wins when loaded from real java.base" — a
   rationale that does not actually apply here, since `StringReader` never
   has real bytecode per the class_manager.rs injection above). This
   function's caller chain (`register_string_rw_natives` →
   `register_nio_natives`/`register_io_natives`) is — like the `ByteBuffer`
   case — only reached from `vm/src/vm/vm_init.rs` inside
   `#[cfg(feature = "synthetic-jdk")]`, which is **not** in the default
   feature set. Dead code in a plain release build.

**Ruled out as a general VM bug** (both work correctly in isolation,
confirmed with dedicated probes): plain field post-increment
(`str.charAt(next++)`-shaped access), `synchronized` instance methods, and
`String.charAt()` — none of these reproduce the "never advances" symptom on
their own. The bug is specific to however `java.io.StringReader` actually
gets resolved at runtime, which — as with the sibling `ByteBuffer` bug —
could not be located.

## Next steps

Same as [`bytebuffer-address-unset-aioobe.md`](bytebuffer-address-unset-aioobe.md):
find the actual live dispatch site (likely requires instrumenting
`vm/src/runtime/interpreter.rs`'s native-call resolution directly, or
finding a registry/cache CratonVM consults that isn't
`shared.native_methods`). Once found, apply **both** fixes together:
1. The index-arithmetic fix in `phases_early.rs` (`ch.len_utf8()` instead of
   `ch` as a byte index) — correct regardless of which registration turns
   out to be live, since the same bug pattern (advance-by-codepoint-value
   instead of by-UTF8-width) may exist in the `read([CII)I` bulk-array
   overload too (not yet audited).
2. Or, if `native-io`'s already-correct 3-field version turns out to be
   reachable via a different route than assumed, prefer that one — it's a
   more faithful match to the field layout `class_manager.rs` documents.

Worktree: `/data/data/wt-valveconn-reverify` on the Azure host (branch
`investigate/valveconn-reverify-20260709`), left in place. Repro Java files:
`StringReaderProbe.java`, `StringReaderProbe2.java`, `PostIncProbe.java`,
`SyncPostIncProbe.java` in `/data/data/` on that host.
