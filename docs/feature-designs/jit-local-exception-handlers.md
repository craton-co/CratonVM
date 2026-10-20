# JIT support for local exception handlers (RBC.6)

**Status:** Shipped (default on; `CRATONVM_JIT_NO_EXC_TABLE_C2` restores the
old blanket refusal). One named residual keeps some methods on the single-pass
backend.

## What it does today

A method that combines `athrow` with a non-empty local `exception_table`
**compiles**. The old blanket gate —
`scan.has_athrow && !exception_table.is_empty()` ⇒ refuse — is gone from
`jit/src/lib.rs`.

What replaced it is not new codegen. Runtime dispatch of a thrown exception to
an in-method handler already existed
(`route_jit_exception_through_method`, `callee_has_exception_table`,
`route_implicit_exc_through_callee`, `mic_callee_has_exception_table` across
`vm/src/runtime/interpreter.rs`, `.../jit_bridge.rs` and
`.../exception_dispatch.rs`). The change is a **gate relaxation plus a
compile-time dataflow safety check** —
`local_handler_reads_unsafe_local(code, code_len, exception_table,
method_descriptor, is_static)`, a CFG-based analysis with a `[rbc6-dbg]`
diagnostic — that closes the correctness gaps the relaxation would otherwise
expose. No CFG changes to the emitted machine code.

## What is not built yet

- **When the safety check fires** — a handler reads a non-parameter local — the
  method is pinned to the single-pass backend. The IR lowerer has no
  precise-handler-frame equivalent (`precise_exception_frames` in
  `jit/src/lib.rs`), and the same condition also disables single-pass scalar
  replacement for that method (`jit/src/x64/driver.rs`). Lifting this is IR
  work, not exception-handling work.

## What actually happened

`jit/src/lib.rs::try_compile_inner` refused to compile any method combining
`athrow` with a non-empty local `exception_table` (gate name in the source:
`RBC.6`, landed `8d0f029ed`, 2026-06-11):

```rust
if scan.has_athrow && !cached.exception_table.is_empty() {
    *backend_attempted = true;
    return None;
}
```

The very next day (`82b9bdf62`, 2026-06-12, "Tomcat bugs H & I"), and in
several follow-ups since (KCFULL-13, Round-8/9/10/11, `549a2c161`
"wildfly bug-05 this/params restore"), this codebase built — for a
*different*, narrower bug (methods that *declare* a handler but contain no
local `athrow`, e.g. `try { risky(); } catch (X e) { recover(); }`) —
exactly the missing dispatch mechanism, generically, and nobody revisited
this gate to notice it now covered the `athrow` case too:

- `x64.rs`'s `has_dispatch` computation unconditionally forces
  `has_dispatch = true` whenever `compiler.emitted_athrow` is set, so any
  method containing `athrow` is *always* entered through
  `execute_jit_call`'s slow, dispatch-aware path — never the raw fast path
  that would leak the sentinel as a return value.
- That slow path (`vm/src/runtime/interpreter.rs::execute_jit_call`) drains
  `JIT_PENDING_EXCEPTION`/NPE/AIOOBE/arithmetic after every JIT return and,
  whenever the just-invoked method declares a non-empty `exception_table`,
  routes the exception through `route_jit_exception_through_method` — a
  *real* handler search: typed catch-class matching with subclass checks,
  first-match-wins declaration order, `finally`/catch-all handling,
  restoring `this` + declared params into a fresh interpreter frame pushed
  at the resolved handler pc with the exception object on the operand
  stack. This fires identically whether the pending exception came from an
  `athrow` inside the just-invoked method or propagated up from something
  *it* called.
- `callee_has_exception_table` / `route_implicit_exc_through_callee` /
  `mic_callee_has_exception_table` provide the equivalent routing for
  JIT-to-JIT direct calls.

This was the confirmed root cause of `Response.toAbsolute()` — Tomcat's hot
URL-normalization path, shaped
`try { ... } catch (IOException) { throw new IllegalArgumentException(...) }` —
permanently staying interpreted.

## The fix — two parts

### Part 1: relax the compile-time gate

`jit/src/lib.rs::try_compile_inner`: delete the unconditional
`if scan.has_athrow && !cached.exception_table.is_empty() { return None }`
bail. The IR (optimizing) path is unaffected — it has its own independent
`cached.exception_table.is_empty()` admission check (`ir::ir_compatible`)
that already excludes any exception-table-bearing method and continues to
do so; only single-pass (`x64::compile`) eligibility widens.

### Part 2: a new compile-time safety check — REQUIRED, not optional

Relaxing the gate alone is **unsound**. `route_jit_exception_through_method`
reconstructs the handler's interpreter frame from **only `this` + the
method's declared incoming params** (a documented, pre-existing limitation
of that function). A handler — or any code reachable after it, within the
*same* method — that reads a local variable NOT in that set observes a
stale zero/null instead of whatever the compiled code's real execution
actually computed.

**This was not a hypothetical risk.** A differential bisection repro,
`AthrowCountBisect.twoThrowsSequential` (`vm/tests/jit_local_exception_handler_tests.rs`):

```java
static int twoThrowsSequential(int x) {
    int a;
    try {
        if (x == 0) throw new RuntimeException();
        a = 1;
    } catch (RuntimeException e) { a = -1; }
    int b;
    try {
        if (x == 1) throw new IllegalStateException();
        b = 2;
    } catch (IllegalStateException e) { b = -2; }
    return a + b;
}
```

— two sequential, non-nested try/catch blocks, where the SECOND handler's
own code reads `a`, last assigned by the FIRST try's successful path —
compiled cleanly under a gate-relaxation-only build and **silently produced
a wrong checksum** (`7386`/`7354`/`13674` across runs — nondeterministic,
timing-dependent on exactly when JIT compilation kicked in — vs. the golden
`19998` computed on a real JDK). This is not specific to `athrow`: the same
frame-reconstruction gap in `route_jit_exception_through_method` equally
affects the **pre-existing** "declares a handler, no local `athrow`"
population that has been compiling since BUG-H (2026-06-12) — this bisect
would reproduce identically today on unmodified `dev` using only implicit
exceptions with two sequential try/catch blocks with a similarly-shaped
dependency, older than this fix and unrelated to it.

**The safety check** (`jit/src/lib.rs::local_handler_reads_unsafe_local`,
called for *every* method with a non-empty `exception_table` — both
populations, not just the newly-relaxed one): for each exception-table
entry, walk the method's `*load`/`*store`/`iinc` instructions (recorded
during `jit_scan` as `JitScanResult::local_slot_ops`, zero new
pc-advancement logic — just a side-effect recorded in the scanner's already
-correct per-opcode arms) in ascending bytecode-pc order **starting at that
entry's `handler_pc`**, tracking a `safe` set of local slots (initially
`this` + declared params). A store adds its slot to `safe`; a load of a
slot not yet in `safe` means this method is unsafe — the whole method bails
(falls back to the pre-existing "stay interpreted" behavior, exactly as if
RBC.6 had never been relaxed for it).

This is deliberately **control-flow-insensitive** (walks raw bytecode in pc
order, ignoring every branch/goto/switch target) — a conservative
over-approximation:

- **Sound**: if the scan finds no unsafe load, no real control-flow path
  can hit one either, because every real path's instruction sequence (for
  the portion at `pc >= handler_pc`) is a sub-selection of the same
  pc-ordered instructions already verified safe.
- **Not complete**: it can reject some methods that are actually safe (a
  local written on every real path before it's read, where a backward
  branch means the write's pc is *lower* than a load reachable from later)
  — acceptable, since those methods simply keep the pre-existing
  "stay interpreted" behavior — no regression, no wrong answer, just a
  missed optimization in a rarer shape.
- Applying it to `twoThrowsSequential`: handler #2's own linear scan starts
  at its own `handler_pc`, never having seen the earlier store to `a`
  (which happened during handler #1's — or the first try's — own,
  earlier-in-pc-order code) — correctly flagged unsafe, method bails.
  Applying it to `Response.toAbsolute()`'s shape (single try/catch, handler
  stores the caught exception then immediately rethrows using only that
  local + a declared param) — no unsafe load found, method compiles.

### Part 3 (bundled into the same fix, not a separate follow-up): PC-precise `throw_pc`

Even with Part 2, a **second**, narrower correctness gap remained reachable:
`execute_jit_call` previously always called `route_jit_exception_through_method`
with `throw_pc = usize::MAX` ("unknown"), under which a *typed* handler
matches purely by exception class regardless of where in the method the
exception actually originated. For a method with 2+ exception-table entries
whose catch types are in a subtype relationship (`twoThrowsSequential`'s
`RuntimeException` vs. later `IllegalStateException`, a `RuntimeException`
subclass), this let the **wrong** entry win by declaration order — observed
directly via `CRATONVM_DBG_RBC6=1` tracing: all 12640 routing calls for
`twoThrowsSequential` resolved to `handler_pc=Some(17)` (entry #1) even for
the ~6667 calls that actually threw from try-block #2.

Fixed by threading the ONE case where the pc IS known at JIT-compile time —
a local `athrow`'s own bytecode pc (`self.cur_bc_pc`/`pc` in the x64 codegen
loop) — through to the routing call:

- New `JitSignals::athrow_bci: Cell<i64>` (`vm/src/jit/helpers.rs`, `-1` =
  unknown), drained alongside `.exception` by `take_all_jit_signals` /
  `DrainedJitSignals::athrow_bci`.
- `jit_throw_exception` gained a second parameter, `bci: i64`, and stashes
  it via the new `set_jit_pending_exception_with_bci`. The x64 `athrow`
  (0xbf) codegen arm now passes its own bci as an immediate second argument
  (`ARG_REGS[1]`).
- Every OTHER site that stashes a general pending exception (re-stash on
  OSR bail, a callee-propagated exception via `jit_invoke_dispatch`) always
  resets `athrow_bci` to `-1` — only the direct-local-athrow path can ever
  set a real value, so a re-stashed or propagated exception conservatively
  falls back to the pre-existing `usize::MAX` behavior (correct, if less
  precise).
- `execute_jit_call` and its sibling `execute_jit_call_decoded` now pass
  `sig.athrow_bci as usize` instead of `usize::MAX` when `sig.athrow_bci >=
  0`.

With Part 3 alone (no Part 2), `twoThrowsSequential` correctly routes each
exception to its own entry — but STILL produces a wrong checksum, because
handler #2's own code still reads `a`, a local the frame reconstruction
never restores. Part 2 (the safety check) is what actually makes this
method's compilation refuse — Part 3 fixes a real, narrower, independently
-confirmed bug (wrong-handler selection) that would otherwise remain latent
for methods that DO pass the Part 2 safety check but still have 2+
exception-table entries with related catch types and pc-unaware routing
(e.g. two SEQUENTIAL try/catch blocks whose handlers only touch params —
Part 2 would admit such a method, and it would need Part 3 to route
correctly). Both parts are required together for full correctness; neither
alone is sufficient.

## A separate, unrelated, pre-existing limitation found (NOT fixed, out of scope)

While building a minimal differential repro of `Response.toAbsolute()`'s
exact shape using custom exception types with string-concatenated messages
(`throw new Outer("wrapped" + i, inner)`), that specific repro method
*continued* to bail even after both fixes above — traced (via
`CRATONVM_DBG_RBC6=1` + `CRATONVM_DBG_JITC=1`) to an entirely different,
pre-existing gate: the "BUG-LQB-SCOPE" check in `jit_scan`'s `invokedynamic`
(0xba) arm (`jit/src/x64.rs`), which refuses to compile a method containing
`invokedynamic` (used for string concatenation by modern `javac`) if a
"committing side effect" (`invoke*`/`putfield`/`putstatic`/array-store)
occurs earlier in raw bytecode-pc order — because the single-pass backend's
`invokedynamic` lowering is an unconditional deopt trap, and re-running
from the interpreter's entry would double-execute that earlier side effect
(a real, already-tracked, already-documented Liquibase `Scope` corruption
history — see the comment at that gate's declaration site).

My synthetic repro's TWO `athrow` sites each construct an exception with a
string-concatenated message (2 `invokedynamic` sites total), with an
`invokespecial <init>` call (a "committing side effect") between them —
exactly the shape this unrelated gate rejects. **Confirmed not to affect
the actual `Response.toAbsolute()` target case**: rebuilding the identical
repro with plain (non-concatenated) exception messages — matching
`toAbsolute()`'s real `throw new IllegalArgumentException(location, ioe)`,
which passes `location` straight through, no concatenation — compiles
cleanly (`full-compile ... len=723`) and produces the correct checksum.
Left as a known, separately-tracked, out-of-scope limitation; worth a
dedicated pickup someday (the double-execution risk this gate protects
against only applies when the interpreter *re-runs the whole method*,
which is no longer the only fallback shape now that
`route_jit_exception_through_method`/real-frame-deopt precise-resume exist
for some cases — but auditing whether it's actually safe to relax is a
separate investigation, not bundled into this fix).

## Closing note (2026-08-17): the machine-code half is not missing, only gated

RBC.6 relaxed the *compile* gate for methods with local exception handlers and
added no new codegen, which left a reasonable-sounding conclusion behind: that
such a callee still could not be reached by a compiled caller's inline cache,
and that the codegen to make it safe had yet to be written.

It had already been written, for a different reason.
`Compiler::emit_inline_callee_deopt_check` (`jit/src/x64/deopt_stubs.rs`) is
emitted after every inline direct-entry `CALL` — each PIC slot, the MIC arm, the
megamorphic hashed stub's twin, and the baked `invokestatic`/`invokespecial`
direct calls — and hands the `i64::MIN` sentinel to `jit_service_callee_deopt`,
which runs the CALLEE's own table. It landed for the H2 `MVMap`/`DataType.read`
case and the exception-table ban was never revisited against it. All that was
left was the gate.

`mic_publish_exception_table_callees()` is therefore default-ON since
2026-08-17, interlocked so it refuses to publish whenever
`CRATONVM_JIT_SP_IC_DEOPT_CHECK` is not `On`. The acceptance test is
`apps/probes/CalleeExceptionTableSemanticsProbe.java`, whose fourth arm — the ban
lifted with that check deleted — is the red proof: an `ArithmeticException`
escapes the callee's own `catch` to `main`. Measurement and the full record are
in `internal/performance/a-compiled-call-goes-out-to-rust-two-causes-RETIRED-20260817.md`.
