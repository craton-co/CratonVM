# JIT support for local exception handlers (RBC.6)

Status: **NOT STARTED** — problem statement + scoping only, written 2026-07-19
after root-causing `docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`'s
`TestResponsePerformance` relative-perf residual back to this exact gate.

## Problem & motivation

`jit/src/lib.rs::try_compile_inner` refuses to compile any method containing
both a local exception handler (non-empty `exception_table`) and an `athrow`
instruction (gate name in the source: `RBC.6`):

```rust
// RBC.6 — a method containing `athrow` compiles only when it has NO
// local exception handlers: the athrow lowering stashes the exception
// and returns the deopt sentinel, which cannot dispatch to an
// in-method handler. Permanent for this bytecode -> bail-list it.
if scan.has_athrow && !cached.exception_table.is_empty() {
    *backend_attempted = true;
    return None;
}
```

This is a **correct, deliberate, permanent** refusal — not a bug to patch
around. The JIT's current exception path (both for a local `athrow` and for
an exception propagating up from a callee) only knows how to hand control
back to the *caller* (interpreter or another JIT frame) via the deopt
sentinel. It has no mechanism to redirect control to a handler bytecode
offset *within the currently-executing compiled method*, with the correct
locals/operand-stack state and the exception object bound to the catch
variable. Compiling such a method with the current codegen would silently
skip the local `catch` and let the exception propagate past it — visibly
wrong behavior (wrong exception type escapes, or a `finally`-equivalent
cleanup never runs).

**Real-world cost**: this is not a narrow pattern. `try { ... } catch (X e)
{ throw new Y(msg, e); }` (validate-or-wrap-and-rethrow) is a common Java
idiom. Confirmed instance: `org.apache.catalina.connector.Response
.toAbsolute(String)` — Tomcat's hot URL-normalization path, called on every
redirect/forward/`sendRedirect` — has exactly this shape around its
`CharChunk`-based string building, and as a direct result runs fully
interpreted for its entire body no matter how hot it gets, while every
method it calls compiles and runs fast. Confirmed via
`CRATONVM_DBG_JITC=1`:

```
[cratonvm-jitc] bg-compile org/apache/catalina/connector/Response.toAbsolute(...)... tier=C1
[cratonvm-jitc] compile-bail org/apache/catalina/connector/Response.toAbsolute(...)... backend_attempted=true
```

— a permanent, one-time bail (correctly not a retry storm — see the
existing RBC.4 fix), but permanent in the sense that this method NEVER
benefits from JIT compilation. In `TestResponsePerformance`'s 1,000,000-
iteration microbenchmark this produces a measured ~3x slowdown relative to
an equivalent `java.net.URI`-based implementation that has no such shape
and compiles cleanly — the reverse of HotSpot's own relative ordering for
the same two approaches (HotSpot doesn't have this limitation, so
`toAbsolute()`'s `CharChunk` path is `~2-4x` *faster* there, per the
test's own source comment).

## Two possible scopes

### Option A (general): full in-method exception-handler dispatch

The JIT would need to, at every call site or `athrow` inside a `try`-range:
1. On exception (from a callee's propagated failure, or a local `athrow`),
   look up whether the *current* PC falls within a `try`-range that has a
   matching handler (by exception type — requires a runtime `instanceof`
   check against the handler's catch-type, matching JVM multi-catch/
   supertype semantics).
2. If so, transfer control to the handler's bytecode offset **within the
   same compiled frame**: reconstruct the operand stack to `[exception]`
   (JVM spec: a handler always starts with exactly the exception object on
   an otherwise-empty stack) and preserve the locals as of the handler
   entry, then resume codegen'd execution there — NOT a deopt to the
   interpreter, an honest in-frame jump.
3. If no local handler matches, THEN fall back to the existing deopt-
   sentinel propagate-to-caller path (already correct/implemented).

This needs real register-allocator support for handler entry points as
additional CFG merge points (today the compiler almost certainly treats
`try`-protected regions as pure linear/branchy code with no handler-entry
edges in its liveness/allocation model — confirm against `jit/src/ir.rs`'s
CFG builder and `jit/src/x64.rs`'s single-pass allocator before starting).
Full generality also needs multi-catch (multiple `catch` clauses on one
`try`) and correct `finally`/`try-with-resources` (`jsr`/`ret` is gone
since Java 6, but `finally` compiles to duplicated cleanup code the
verifier already normalized — should already look like plain control flow
to the JIT, worth confirming with a bytecode dump on a `try-with-resources`
sample before assuming it's free).

### Option B (narrower, recommended starting point): handlers that never re-enter the try region

Restrict to the common case — and the one `Response.toAbsolute()` hits — 
where every local handler's body has no control-flow edge back into its own
(or any enclosing) `try`-range: the handler either always returns, always
`athrow`s, or always falls through to code strictly after the `try`-range.
In this shape, the handler is effectively a second, disjoint exit path with
no re-entrant complexity — the compiler only needs:
1. Handler-entry as a CFG merge point with the JVM-spec `[exception]` stack
   state (same requirement as Option A, but only for this shape).
2. No requirement to model control returning to the middle of the `try`
   body — so no interaction with loop/backedge codegen, OSR, or the
   existing deopt-sentinel machinery for the *try*-body's own guards.

This is very likely the majority real-world shape (validate-or-wrap-and-
rethrow, log-and-return, resource-cleanup-then-rethrow) and should cover
`Response.toAbsolute()` and most Tomcat/Spring/JDK-internal code without
needing the full generality of Option A. Static detection: after building
the exception-table-aware CFG, verify no handler's reachable node set
intersects any `try`-range's *protected* node set for a range enclosing or
overlapping that handler's own try (i.e. no path from the handler visits
protected bytecode again). Methods that don't fit this shape keep bailing
via the existing RBC.6 gate — this is additive, not a replacement.

## Where to start (codebase citations)

- **Gate to relax**: `jit/src/lib.rs`, `try_compile_inner`, the RBC.6 block
  (search `has_athrow && !cached.exception_table.is_empty()`).
- **CFG/scan pass**: whatever builds `scan` (the `has_athrow` flag) in the
  same function — likely a bytecode scanner pass; extend it to also compute
  the "handler never re-enters try" predicate per Option B, or hand off to
  `jit/src/ir.rs` if CFG construction already happens there for the IR path
  (`ir::ir_compatible` is checked separately, single-pass `x64::compile`
  is the actual target for most methods per `wire-tiered-manager.md` — this
  feature most likely needs to land in the single-pass backend first, IR
  path second, matching that doc's own C1/C2 staging convention).
- **Existing exception plumbing to reuse, not reinvent**: the interpreter's
  own exception-table walking (`vm/src/runtime/interpreter.rs` — search for
  the interpreter's own `athrow`/exception-dispatch handling) is the
  reference implementation for correct catch-type-match semantics
  (`instanceof` against the handler's catch class, first-match-wins,
  re-throw-if-none-match) — the JIT's handler-entry dispatch must match it
  exactly (a mismatch would misroute an exception to the wrong handler or
  the wrong outer scope, a correctness bug, not a perf one).
- **NOT reusable**: `vm/src/runtime/deopt_materialize.rs` / "real-frame
  deopt" (`docs/feature-designs/deopt-osr.md`) — checked as a candidate
  foundation, but it's for scalar-replacement re-materialization after a
  *type-speculation* guard failure, unrelated to exception dispatch, and is
  itself still default-off/experimental. No overlap to exploit.

## Validation plan (before landing, whichever option)

- Bytecode-level unit tests exercising: single catch + unconditional
  rethrow (the `toAbsolute()` shape); catch + return; catch + fall-through;
  (Option A only) catch + code that re-enters the try body; multi-catch;
  nested try/catch; `try-with-resources`/synthetic `finally` duplication;
  an exception thrown from *within* the handler itself (must NOT be caught
  by the same handler, must propagate per JVM spec — a classic dispatch-
  logic off-by-one).
- Differential test against the interpreter (and ideally the existing
  `differential-fuzzer.md` infra) for exception TYPE, message, and stack
  trace depth, not just "did the test pass" — silently catching the wrong
  exception type or losing frames is not caught by many app-level tests.
- Re-run `TestResponsePerformance` at the doc's canonical `-Xmx2g` as the
  acceptance target once landed: `Assert.assertTrue(homebrewWin ==
  winTarget)` should flip to passing once `toAbsolute()` compiles and its
  per-call cost drops to roughly its callees' already-JIT'd speed.
