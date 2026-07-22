# BUG-H — JIT-compiled method's local `catch` is bypassed on a JIT→JIT call (implicit exceptions escape)

> **STATUS: FIXED** (branch `fix/tomcat-jit-exc-bugs-hi`, commit 82b9bdf6).
> `TestHexUtils` now passes (was 1 failure). The blunt "don't compile any
> exception-table method" approach was NOT used (too broad). Instead the fix
> routes a callee-thrown implicit exception through the **callee's own**
> exception table on every JIT→JIT path, by re-executing the throwing callee in
> the interpreter (where exception dispatch is correct) rather than compiling a
> wrong in-JIT handler. See "## Fix" below. No regression: `TestB2CConverter`,
> `TestByteChunk`, `TestAscii` stay green; bench fib/sieve/matrix + bintrees18
> checksums match HotSpot.
>
> **Note on the other two tests originally listed here** (`TestCharsetUtil`,
> `TestHttpParserHost`): these were mis-attributed to BUG-H. They do NOT fail by
> the implicit-exception-escape mechanism — at baseline (no fix) they produce
> assertion failures (wrong *values*), not uncaught-AIOOBE aborts, and they are
> unchanged by this fix. `TestCharsetUtil` (1 fail) is the charset-decode
> [BUG-K](BUG-K-charsetdecoder-endofinput.md); `TestHttpParserHost` (92 fails) is
> a separate JIT value-miscompile in the Host parse state machine (still open).

**Tests:** `org.apache.tomcat.util.buf.TestHexUtils`,
`org.apache.tomcat.util.buf.TestCharsetUtil`,
`org.apache.tomcat.util.http.parser.TestHttpParserHost` (and any hot method that
catches an implicit runtime exception). HotSpot: PASS.
**Symptom under CratonVM (JIT on only):** an `ArrayIndexOutOfBoundsException`
escapes a `catch (ArrayIndexOutOfBoundsException)` and aborts the test; passes
with `CRATONVM_DISABLE_JIT=1`.

## Minimal reproducer

```java
static int[] T = new int[55];
static int get(int i)    { try { return T[i - 48]; } catch (ArrayIndexOutOfBoundsException e) { return -1; } }
static int caller(int i) { return get(i); }
// warm caller() + get() hot so BOTH JIT-compile, then:
caller(0);   // T[-48] -> AIOOBE -> ESCAPES get()'s catch (should return -1)
```

`HexUtils.getDec` is exactly this shape (`return DEC[index - '0']` inside a
`catch (ArrayIndexOutOfBoundsException)` returning -1).

## Root cause

The JIT codegen lowers **both** an explicit `athrow` and **implicit** runtime
exceptions — array-index-out-of-bounds (`jit_iaload`/`jit_iastore`/…), null
dereference, divide-by-zero — to *"stash the pending exception + return the
`i64::MIN` deopt sentinel"*. That pending exception is converted into a real
throw and routed through the **method's own exception table only at the
interpreter↔JIT boundary** (`execute_jit_call` in `interpreter.rs`, which drains
`JIT_PENDING_AIOOBE`/`JIT_PENDING_NPE` and calls
`route_jit_exception_through_method`).

When a JIT-compiled method is invoked **directly from another JIT-compiled
method**, there is no interpreter boundary between them. `get()` returns to
`caller()`'s machine code with the pending-AIOOBE flag set; nothing drains it
through `get()`'s exception table, so `get()`'s in-method `catch` never runs.
The flag is finally observed at the next unrelated interpreter boundary and
routed through the *wrong* frame (which has no matching handler), so the
exception escapes uncaught.

The pre-existing gate only refused to compile methods with an explicit `athrow`
plus a non-empty exception table; implicit-throw methods like `getDec` (no
`athrow`) slipped through.

## Fix (shipped)

Methods with exception tables **still JIT-compile** (no broad blast radius). The
fix ensures that whenever such a callee throws an implicit exception across a
JIT→JIT boundary, the exception is routed through the **callee's own** table —
by re-executing the throwing callee in the interpreter (whose exception dispatch
is correct), reusing the existing `bail_to_interpreter` machinery. Three sites,
one per JIT→JIT path:

1. **Direct machine-code static call** (`vm/src/runtime/interpreter.rs`,
   `callee_compiler` closure): refuse to register a direct `CALL` into a callee
   that declares a non-empty exception table; the site drops to the dispatch
   helper (`jit_invoke_dispatch`) instead. The direct `CALL` re-enters no Rust,
   so it cannot be intercepted — gating it is the only correct option.

2. **Dispatch-helper fast paths** (`vm/src/jit/helpers.rs`,
   `jit_invoke_dispatch` dcache/jcache/post-compile): after a compiled callee
   returns the `i64::MIN` sentinel with a pending AIOOBE/NPE, if the callee has
   an exception table, re-execute it in the interpreter
   (`route_implicit_exc_through_callee`) so its handler runs (or the exception
   propagates correctly if uncaught).

3. **Virtual MIC/PIC** (`vm/src/jit/helpers.rs`, `jit_invoke_virtual_mic`):
   never publish a direct inline-cache entry for a receiver-resolved callee with
   an exception table (the inline machine-code cascade in `jit/src/x64.rs` would
   `CALL` it directly, bypassing its table). Such calls stay on the helper's
   `invoke_or_native` (interpreter) path. Covers instance methods like
   `HttpParser.isNotRequestTargetRelaxed`.

The hot bench methods (bintrees `bottomUpTree`/`itemCheck`, etc.) have no
exception table and are untouched; bintrees18 checksum still matches HotSpot.
A standalone reproducer is in `bench/BugH.java`.
