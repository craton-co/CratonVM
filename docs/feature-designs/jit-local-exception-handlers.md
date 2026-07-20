# JIT support for local exception handlers (RBC.6)

Status: **Compile-time correctness FIXED and validated end-to-end against
the real Tomcat suite fixture** (2026-07-19, two sessions). `Response
.toAbsolute()` — the doc's own motivating case — now JIT-compiles and
produces correct results, confirmed directly, not inferred. The session-2
recompile-storm residual (repeated recompilation, 86-94 times per run) is
now **FIXED** (2026-07-20 session — see "2026-07-20 session" below): the
root cause was `DeoptReason::OsrExit` being routed through the generic
mis-speculation recompile policy instead of being treated as the real,
expected, structurally-unfixable-by-recompiling event it is, compounded by
a `MakeNotCompilable` give-up decision only updating one of two independent
JIT-eligibility registries. `toAbsolute()` now compiles at most once per
process. **Still OPEN**: `TestResponsePerformance`'s relative-perf assertion
itself still fails, but for a different, deeper, newly-characterized reason
unrelated to recompilation — see the 2026-07-20 section for the full
diagnostic and why it looks like it may be an unrelated `dev` regression.
The original problem statement (written
session 1, kept at the bottom for history) assumed the JIT had *no*
mechanism to dispatch a thrown exception to an in-method handler and
scoped a substantial new compiler feature (Option A/B) to build one. That
assumption was wrong by the time it was written — the actual fix is a
compile-time gate relaxation plus new compile-time *safety* checks that
close real, demonstrated correctness gaps the relaxation would otherwise
expose. No new codegen, no CFG changes to the compiled machine code
itself — only scan-time/dataflow analysis.

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

Confirmed root cause of `Response.toAbsolute()` (Tomcat's hot
URL-normalization path, `try { ... } catch (IOException) { throw new
IllegalArgumentException(...) }`) permanently staying interpreted — see
`docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`'s
2026-07-19 deep-dive section for the original trace that led here.

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

## Also fixed in passing (pre-existing, unrelated, discovered while validating)

- Seven `jit/tests/*.rs` fixtures had not been updated for the `ldc_string`
  field added to `JitRuntimeHelpers`, and `intrinsic_arraycopy.rs`'s
  `compile_with_param_slots` call was missing the `ldc_string_info` and
  `indy_info` parameters added since it was last touched — both
  compile-time breaks, blocking `cargo test -p cratonvm-jit` entirely. A
  recurring pattern in this codebase: a field/param is appended to a
  struct/function signature and hard-initialized test literals elsewhere
  aren't updated to match.

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

## Validation

- `cargo test -p cratonvm-jit --release`: 915 lib unit tests + all
  integration suites (`differential`, `ir_vs_singlepass`, `intrinsic_*`)
  green, no regressions from either fix.
- `vm/tests/resources/cratonvm/JitLocalHandler.java` +
  `vm/tests/jit_local_exception_handler_tests.rs`: six shapes, each driven
  20000 calls deep (past the default JIT hotness threshold of 500) —
  single catch + unconditional rethrow-as-different-type (the
  `toAbsolute()` shape), catch + return, catch + fall-through, multi-catch,
  nested try/catch, exception-thrown-inside-handler-must-not-be-recaught.
  Expected checksums computed by running the byte-identical fixture under a
  real JDK. (These `vm`-crate tests require `javac` to auto-compile fixture
  `.java` files via `build.rs`; this session's local Windows environment
  could not run them end-to-end — an unrelated, pre-existing environment
  gap, `javac failed` on an unrelated `.java` fixture needing an external
  LDAP SDK jar not present locally — so validation instead ran the built
  `cratonvm.exe` binary directly against manually-`javac`-compiled copies
  of the same fixtures, comparing output byte-for-byte against real-JDK
  golden values. Should be re-run via `cargo test -p cratonvm-vm` in an
  environment with the full dependency set.)
- `AthrowCountBisect.oneThrow`/`twoThrowsSequential` bisection: confirmed
  the exact failure mode, confirmed both fixes close it (correct checksum
  `19998`, method now safely stays interpreted per the Part 2 gate — traded
  a perf opportunity for guaranteed correctness, the right trade-off for a
  rare shape).
- `Response.toAbsolute()`'s exact shape (single try/catch, unconditional
  rethrow of a different type, no string-concat in the handler) now
  compiles (`full-compile`) and produces correct results under
  `CRATONVM_DBG_JITC=1`; see the known-issues doc for the
  `TestResponsePerformance` re-run once merged and the Tomcat suite fixture
  is reachable.

## 2026-07-19 session 2 (Linux build host, real Tomcat fixture validation)

Picked this back up per an explicit "fix all issues related to this doc and
their residuals completely" directive, with SSH access to the Azure Linux
build host (`/data/data/cratonvm` main checkout, `/data/data/apps/tomcat`
suite fixture — same host referenced throughout this codebase's other
known-issues docs). Worktree: `/data/wt-rbc6-linux-validate-20260719`,
branch `validate/rbc6-linux-perf-20260719`, branched from `origin/dev`
(which already had session 1's merge, `dfc03eb68`/`c4252a6c1`).

### Re-running `TestResponsePerformance` found TWO more real, blocking bugs

Session 1 validated correctness thoroughly but could not reach the actual
Tomcat suite fixture (Windows-local environment, no access to the remote
host). Doing so now immediately surfaced that `Response.toAbsolute()`
**still did not compile** — the session-1 fix was necessary but not
sufficient for the doc's own motivating case:

**Bug 1 — `local_handler_reads_unsafe_local` was itself over-conservative.**
The session-1 safety check scanned linearly from each handler's
`handler_pc` all the way to the END OF THE METHOD, never stopping at the
handler's own `athrow`/`return` terminator. `Response.toAbsolute()`'s real
bytecode has a short first handler (`catch (IOException) { throw new
IllegalArgumentException(location, ioe); }`, ending in `athrow` at pc 94)
followed immediately by *unrelated* code belonging to a completely
different branch of the method (checking `leadingSlash` at pc 95, a local
never touched by this handler). The scan kept going past the handler's own
terminator into that unrelated code, found a load of a non-param local
there, and rejected the whole method — confirmed via
`CRATONVM_DBG_RBC6=1` tracing (`UNSAFE handler_pc=82 at_pc=95
unsafe_slot=2`) run directly against the suite fixture.

This also exposed a second, latent, never-triggered soundness gap in the
SAME check: the raw-pc-order "is there some store to this slot at a lower
pc" test is not path-sensitive — `if (c) { x = ...; } else { read x; }`
inside a handler would have javac emit the store (true branch) at a lower
pc than the read (false branch) regardless of which branch actually runs,
so the old check could have wrongly ACCEPTED an unsafe method with that
shape (never observed in practice, but a real gap).

**Fix**: replaced the raw-pc-order approximation with a real, CFG-based,
dominance-respecting forward "definitely assigned" dataflow —
`regalloc::handler_has_unsafe_local_read` (`jit/src/regalloc.rs`), reusing
that module's own already-trusted `bc_len`/`branch_target`/
`is_unconditional`/`switch_targets` bytecode-CFG helpers (the exact ones
`build_cfg` itself uses for register allocation) so the new check can never
diverge from the backend's own understanding of the method's control flow.
Standard worklist dataflow: a slot is safe at a program point only if every
predecessor's propagated safe-set says so (intersection at merges,
monotonically shrinking, guaranteed to terminate). `local_handler_reads_
unsafe_local` (`jit/src/lib.rs`) now just seeds this with `this` + declared
params per handler and delegates. Both bugs above are fixed by
construction — a load is now judged only against what's dominance-reachable
by a store, not "anything scanned so far, or ever."

**Bug 2 — `BUG-LQB-SCOPE`'s premise, like RBC.6's, was stale.**
Even with Bug 1 fixed, `toAbsolute()` still bailed — this time at the
`jit_scan` level, in the unrelated "BUG-LQB-SCOPE" gate
(`jit/src/x64.rs`'s `0xba`/`invokedynamic` scan arm), which refuses to
compile a method containing `invokedynamic` (used for `int`-to-`String`
concatenation — `toAbsolute()`'s port-number formatting, `":" + port`) if a
"committing side effect" (any `invoke*`/`putfield`/`putstatic`/array-store)
occurred earlier in raw bytecode-pc order — believing the trap's fallback
was an imprecise whole-method re-run that could double-execute that earlier
side effect (the real, once-confirmed Liquibase `Scope` corruption this
gate was written to fix, `752796a0a`, 2026-07-07).

Git archaeology showed this premise died the same day it was born: a
**concurrent** fix on a different branch, merged the same day
(`docs/internal/jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md`),
closed four separate bugs in the reason-8 (`UnreachedCode`) trap's
UNCONDITIONAL precise-resume machinery (`emit_osr_exit_map_at_reason`,
`emit_deopt_stubs`'s reason-8 routing — not gated behind
`deopt_real_enabled()`) — including, as its own "FOURTH bug", the *exact*
nested-compiled-callee identity-mismatch scenario BUG-LQB-SCOPE's own
author was worried about (`try_resume_trapped_callee` + baked `method_key`
identity checks, a dedicated repro going from 30000/30000 corrupted calls
to 0/30000). Two independently-authored fixes for the same underlying
problem, from two different angles, landed hours apart, and nobody
reconciled them — the compile-time gate became pure redundant
over-caution once the runtime got precise.

**Fix**: removed the `first_committing_side_effect_pc` tracking and its
one consumer in the `0xba` scan arm. **Validated empirically**, not just
by re-reading the git history: a differential repro mirroring the ORIGINAL
Liquibase `Scope.enter()` shape as closely as possible
(`LiquibaseScopeBisect.java` — `counter++` immediately followed by a
string-concat `invokedynamic`, 3,000,000 calls, both a direct-caller and a
nested-JIT-compiled-caller variant) produced `counterAfter=3000000`
(exactly once per call, matching real-JDK ground truth) on the fixed
binary, with the method confirmed actually JIT-compiled and the trap site
confirmed actually reachable during the run.

### Result: `Response.toAbsolute()` now compiles — but a NEW, separate performance regression blocks the doc's original assertion

With both bugs fixed, `CRATONVM_DBG_JITC=1` against the real suite fixture
confirms `Response.toAbsolute()` now reaches `full-compile` (verified
directly, not inferred). All 9 differential-suite checksums (session 1) and
the Liquibase bisect continue to match real-JDK golden values on this
build — the correctness work is solid and thoroughly validated end-to-end,
against the doc's own real target method, not just synthetic repros.

However, `TestResponsePerformance`'s relative-performance assertion (the
doc's actual acceptance target) still fails, and for a surprising reason:
home-brew (`toAbsolute()`) time got WORSE, not better — ~450-490
SECONDS per round (vs. the pre-fix, fully-interpreted baseline of ~63-72
SECONDS documented earlier this same day). Root cause is NOT yet found.
What's confirmed:

- `Response.toAbsolute()` is being **repeatedly recompiled** during the
  benchmark — 86-94 distinct `full-compile` events for a single method in
  one run (`len=15451` each — a genuinely large compiled artifact), vs. a
  handful of recompiles total for every OTHER hot method in the same run
  combined. This is NOT going through the normal warmup/retry-stride path
  (`tiered-enqueue`, `invoc_count=...`) — zero such lines appear for
  `toAbsolute()` — it's being triggered some OTHER way, back-to-back, with
  no other trace output interleaved between an event and the next.
- Ruled out: `CRATONVM_DBG_DEOPT=1` shows only compile-time "OSR-exit map
  emitted" snapshot recording (expected, once per compile) — no actual
  runtime deopt/eviction/despeculation events were captured in the sampled
  window, though the sampling was necessarily partial (a live process,
  bounded observation time).
- Investigated and ruled out as *the* cause (though kept as a real,
  independently-justified fix — see below): `mic_callee_has_exception_table`
  excludes ANY callee declaring a local exception table from FOUR separate
  MIC/PIC/direct-call caching fast paths in `vm/src/jit/helpers.rs`. THREE
  of those four are genuinely necessary — their own doc comments correctly
  explain that the INLINE machine-code MIC/PIC cascade (`jit/src/x64.rs`)
  `CALL`s a cached raw entry pointer directly from compiled code, bypassing
  Rust-level exception routing entirely, so publishing an exception-table
  bearing callee's raw entry there really would skip its own `catch`. The
  FOURTH (`direct_virtual_compiled_callee_entry_enabled`'s
  `VIRTUAL_DISPATCH_CACHE`, ~`vm/src/jit/helpers.rs:5261`) is different: it
  is a plain Rust `HashMap` consulted from INSIDE the same Rust dispatch
  helper, and a cache hit still calls `route_implicit_exc_through_callee`
  afterward exactly as a miss would — so excluding exception-table callees
  bought no correctness there. Relaxed it (kept the other three exactly as
  they are). `cargo test -p cratonvm-jit` and the full differential suite
  stayed green, but this did NOT move `TestResponsePerformance`'s numbers
  (still ~330-490s/round, recompile count still 94) — so it was not, or not
  solely, the bottleneck. Kept anyway: it's independently correct and
  should help SOME other exception-table-bearing virtual-dispatch callee
  even though it didn't explain this one.

### Recommendation for whoever picks this up next

The compile-time correctness work (this doc's original subject) is DONE
and thoroughly validated. What remains is a genuinely new, separate
investigation: **why does a large (15KB compiled), twice-exception-table,
now-JIT-compiled method get recompiled 80+ times during a
million-iteration benchmark, and why is its steady-state (or repeatedly-
reset) execution so much slower than plain interpretation?** Concrete next
steps:
1. Find what's actually TRIGGERING each recompile, since it demonstrably
   isn't the normal `tiered-enqueue`/invocation-count warmup path. Likely
   candidates to check next: whether `toAbsolute()`'s artifact is being
   evicted by a class-manager epoch bump unrelated to itself (every OTHER
   method's own tier-up/supersede event was observed immediately
   preceding several of the recompiles in the trace — worth checking
   whether there's a shared "supersede epoch" that spuriously invalidates
   unrelated compiled methods), or whether `has_dispatch`/`can_deopt_resume`
   interact with something specific to a 2-entry exception table.
2. Once recompiles stop, re-measure the STEADY-STATE per-call cost
   directly (e.g. a tight micro-loop calling only `toAbsolute()` after
   letting it settle) to separate "compile-time cost paid repeatedly" from
   "genuinely slow generated code" as two different possible root causes —
   they need different fixes.
3. Given the recompile pattern correlates temporally with OTHER small
   methods `toAbsolute()` calls getting C1→C2 "supersede" events, check
   whether there's an (probably unintended) inlining-invalidation cascade:
   does the single-pass C1 backend inline any of `toAbsolute()`'s trivial
   callees (`getScheme`/`getServerName`/`getServerPort` are prime
   candidates), and if a callee's own tier-up invalidates callers that
   inlined it, does that invalidation logic correctly distinguish "this
   caller genuinely needs invalidating" from "false positive"?

## 2026-07-20 session: recompile-storm root-caused and fixed; deeper residual found

Full account, evidence, and the reverted-merge tangent in
`docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`'s
"2026-07-20 session" section — summarized here for this doc's own history.

**Root cause of the recompile storm**: `toAbsolute()`'s internal loop always
exits back to the interpreter at the same bci (225) — a real `DeoptReason::
OsrExit`, not a mis-speculation. `jit/src/deopt.rs::recommend_action` routed
it through the generic count-based recompile-escalation policy anyway,
so every occurrence evicted the compiled artifact and (once escalated)
eagerly re-queued a fresh, wasted 15KB recompile — the observed 86-94
recompiles/run. Fixed in 3 iterations, the last of which uncovered a second,
independent bug: `DeoptAction::MakeNotCompilable`'s give-up decision only
updated `SharedVm::jit_skip_set` (consulted by the interpreter's own
per-call hotness path), not `cratonvm_jit`'s separate RBC.4 bail-list
(consulted by `try_jit_compile_callee`, the path a JIT-compiled CALLER
actually uses to dispatch to a callee) — so a caller-dispatched give-up
never stuck. `toAbsolute()` now compiles at most once per process,
verified across 3 independent runs; `cargo test -p cratonvm-jit deopt`
green (86 tests, no regressions).

**New residual**: with recompiling fixed, `TestResponsePerformance`'s
`home-brew` time is still ~350-420s/round (vs. the historical
63,500-72,700ms/round baseline) — and this number doesn't move across any
of the 3 fix iterations, nor under a clean `CRATONVM_JIT_DENY=Response
.toAbsolute` compile-time deny that bypasses this session's changes
entirely. That rules out a bug in the deopt-policy fix: the cost is
inherent to `doHomebrew()` (itself JIT/OSR-compiled, independent of
`toAbsolute()`'s own compilability) dispatching to a non-compiled callee via
the generic JIT-to-interpreter fallback path. Since that historical baseline
was ALSO measured with `toAbsolute()` permanently interpreted (the original,
pre-this-doc RBC.6 bail), `doHomebrew()` plausibly had this exact shape back
then too — meaning this may be an unrelated `dev` regression introduced
sometime after 2026-07-19 rather than a new discovery about the JIT's
inherent behavior. Not bisected this session (out of scope — see the
known-issues doc for the full reasoning and next-step recommendation).

## Remaining follow-ups (not blockers for this fix)

1. **Try-local recovery** (widens Part-2-safe coverage). For handlers that
   read a local first assigned inside the try body, restore it precisely
   instead of rejecting the method outright. Natural fit for
   `docs/feature-designs/deopt-osr.md`'s `DeoptimizationPoint`/`FrameState`
   snapshot machinery — emit a snapshot at each try-protected call
   site/`athrow` and have `route_jit_exception_through_method` prefer it
   over the incoming-args-only frame when present. Would let
   `twoThrowsSequential`-shaped methods compile too, not just stay
   correctly interpreted.
2. **BUG-LQB-SCOPE relaxation investigation** — see the section above.
   Separate scope; audit whether the double-execution risk it guards
   against is still real given the precise-resume machinery that has
   landed since it was written.
3. Regression-sweep for other methods that were silently staying
   interpreted purely because of the old RBC.6 gate (any method with an
   explicit local rethrow) — a `CRATONVM_DBG_JITC=1` sweep across the
   existing suites, similar to the `TestResponsePerformance` deep-dive that
   found this gate, could surface more, now that they're safe to compile.

## Original problem statement (superseded, kept for history)

The rest of this section is the original (2026-07-19, same-day) scoping
that concluded the fix required substantial new compiler work (Option A:
full in-frame handler dispatch, Option B: a narrower "handler never
re-enters try" static analysis). Neither was needed — the runtime already
had the dispatch mechanism; the actual gap (and the reason this fix has TWO
parts, not one) was a frame-reconstruction *safety* problem, not a
dispatch-*mechanism* problem. Kept verbatim for the reasoning trail.

> The JIT would need to, at every call site or `athrow` inside a
> `try`-range: on exception, look up whether the current PC falls within a
> `try`-range that has a matching handler (by exception type — requires a
> runtime `instanceof` check against the handler's catch-type, matching JVM
> multi-catch/supertype semantics), and if so, transfer control to the
> handler's bytecode offset *within the same compiled frame*: reconstruct
> the operand stack to `[exception]` and preserve the locals as of the
> handler entry, then resume codegen'd execution there. This needs real
> register-allocator support for handler entry points as additional CFG
> merge points, multi-catch, and correct `finally`/`try-with-resources`
> handling. The narrower Option B restricted this to handlers whose body
> never re-enters the try region (no loop/backedge/OSR interaction), static
> analysis to be done on the exception-table-aware CFG.
