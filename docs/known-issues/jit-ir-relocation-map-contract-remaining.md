# IR relocation map contract — frame side done, publication side remaining

**Status: 🟡 PARTIAL.** The maps, safepoint ids and frame layout are implemented
and exercised (`92b6045e7`). `moving_young_coverage_complete` is deliberately
`false` until shadow publication lands. One step remains, specified below.

## Why the map alone is not enough

`OopMapEntry::frame_slot_offsets` is consumed by
`conservative_roots::scan_oop_slots`, which reads each slot and pushes the
**`ObjectRef` value** into the root vector. Those are *marking* roots: they keep
the object alive but give the collector no way to write the new address back
into the frame. The rewritable homes come from the shadow stack, which the
verifier reads via `published_shadow_values` and cross-checks against the frame
band (`band_has_unpublished_young_word`).

So `moving_young_coverage_complete: true` asserts *publication*, not
*enumeration*, and setting it without publishing is unsound in principle: it
would be safe only because the band scan happens to catch the false claim, which
is soundness resting on the verifier catching a producer's lie.

**Correction — do not repeat this inference.** Attempt 1 also blamed that claim
for a `ZonedDateTimeTest` regression (302 s -> >1200 s), reasoning that a `true`
makes the verifier skip its early-out and band-scan every live frame. Attempt 2
disproved it: with publication fully implemented and the assertion gated OFF,
the timeout remained. The reader side is not the cost. See "Attempt 2" below.

## What remains

Mirror `x64::emit_shadow_push` / `emit_shadow_reload` in `ir_lower.rs`:

1. **Two more reserved slots**, beside the existing sp-id slot: the cached
   `*mut JvmThread` and the push's base `top` (`shadow_savebase_slot_off` in the
   single-pass backend — it makes the reload immune to an intervening unbalanced
   push, see spring-bug-10).
2. **Prologue**: call `helpers.get_current_thread`, store the result to the
   thread slot. Do it *after* the ABI parameter stores — the helper clobbers
   caller-saved registers, and by then the parameters are already in frame
   slots. Gate every use on `get_current_thread != 0`, exactly as the
   single-pass backend does; the JIT unit tests' stub helper table leaves it
   zero and dereferencing it would read stack garbage as a thread pointer.
3. **Push**, immediately before the call, over the offsets already collected
   into `frame_slot_offsets`. Safe to emit where `emit_safepoint_map` is called
   today (top of the `Op::Call` arm): R10/R11/RAX are free there because
   argument staging and the ABI register loads have not happened yet.
4. **Reload**, after the call. The obvious-looking trap here **dissolves**, and
   the resolution matters because it is what makes the rest mechanical:

   * The single-pass backend uses RAX as the scratch temp for frame-resident
     homes, which collides with the return value and would force the reload
     after the result store. **IR does not have to.** RCX (and RDX/R8/R9) held
     outgoing arguments and are dead the instant the call returns, so using
     **RCX as the temp leaves RAX untouched** and the reload can be emitted
     immediately after the call, before anything stores the result.
   * The four routes do not need four insertions. `emit_call_return_check` is
     called as the *first* thing after the call by three of them
     (`emit_direct_cross_call` :1349, `emit_inline_cache_call` :1700, generic
     dispatch :2506) — emit the reload at the top of that function and all three
     are covered at one site. Note it clobbers R10 itself, so the reload (which
     also wants R10) must come first, not interleaved.
   * The fourth route, `emit_self_recursive_call`, does not call it. Simplest
     sound handling: do not push for that route and mark its map not-covered.
     The `invoke_kind == 4` test already happens at the top of the `Op::Call`
     arm, so decide there and pass a `publish: bool` into `emit_safepoint_map`
     (or clear the flag afterwards via `self.oop_maps.last_mut()`).

   Encodings needed, all rbp/R10/R11-relative, none of which exist in
   `ir_lower` yet: `MOV R10,[rbp-d32]` `4C 8B 95`, `MOV R11,[R10+d32]`
   `4D 8B 9A`, `MOV [rbp-d32],R11` `4C 89 9D`, `MOV RCX,[rbp-d32]` `48 8B 8D`,
   `MOV [R11],RCX` `49 89 0B`, `MOV RCX,[R11]` `49 8B 0B`,
   `MOV [rbp-d32],RCX` `48 89 8D`, `LEA R11,[R11+8]` `4D 8D 5B 08`,
   `MOV [R10+d32],R11` `4D 89 9A`, `TEST R10,R10` `4D 85 D2`, `JE rel32`
   `0F 84`. Hand-encoded GC-critical codegen: assert the emitted bytes in a
   unit test before running anything, and validate relocation end to end on a
   quiet host (`cycles=N` with `coverage_fallbacks=0` AND the bt18 checksum
   `68332206`) before flipping the flag.
5. Flip `moving_young_coverage_complete` to the `coverable` value already
   computed in `emit_safepoint_map`.

## Do not "simplify" by skipping the reload

Publishing values without reloading is only sound if the collector treats shadow
entries as PINNED (`CRATONVM_SHADOW_PIN`) — otherwise it relocates the object,
rewrites the shadow copy, and the frame slot keeps the stale address. Pinning
every IR-held reference would also forfeit most of the compaction the contract
exists to enable, so it is a fallback, not the design.

## What already works

With the maps in place and the flag off, `BinTreesClassic 18` at `-Xmx512m`
reports `cycles=25 coverage_fallbacks=0` and returns the HotSpot checksum
`68332206` — the moving young generation copying under live JIT frames. That
path does not depend on IR frames proving coverage; it is what dev's
`CRATONVM_MOVING_YOUNG_NO_JIT` rework unblocked. The IR contract extends the same
guarantee to frames the optimizing tier produces.

## Attempt 2 (same session): publication implemented, then reverted

The publication step above was implemented in full — three reserved slots
(sp-id, cached thread, shadow savebase), a `get_current_thread` fetch in the
prologue, `emit_shadow_push` before each safepoint, `emit_shadow_reload` at the
top of `emit_call_return_check` using RCX so RAX survives, and the
self-recursive route excluded because it bypasses that choke point.

It was correct as far as every fast check goes: `cargo test -p cratonvm-jit`
1060 lib + every integration target 0 failed, `BinTreesClassic 18` returned
`68332206` at both 512m and 2g with `cycles=25 coverage_fallbacks=0`, and the
`--nojit` 128m bt16 returned `14985902`.

**It was reverted because it regresses `ZonedDateTimeTest`: 302 s -> >1200 s.**

The first hypothesis — that asserting coverage makes the verifier skip its
early-out and band-scan every live frame — was WRONG. Gating the assertion off
(`CRATONVM_JIT_IR_RELOC_MAPS`, default off) left the timeout in place, which
rules the reader side out entirely. The cost is on the emission side, in what
the publication machinery adds to every IR method regardless of whether the
claim is made. In rough order of suspicion:

1. **`fetch_current_thread` in the prologue** — a `CALL` on every IR method
   ENTRY, including tiny hot ones. The single-pass backend has a "lazy prologue"
   lever for exactly this (`shadow_pushed_any`: keep the fetch only if the
   method actually publishes something). The IR version fetches unconditionally.
   This is the first thing to try: make the fetch conditional on the method
   having emitted at least one push, patching or NOP-ing it otherwise.
2. the sp-id store at every `Op::Call`;
3. the frame widening (+24 bytes) and the phi zeroing.

`ASTParserLoadingTest` was unaffected throughout (138 s), so whatever it is
scales with call density or method count rather than stack depth.

Bisecting these needs one lever per item and a quiet host; each
`ZonedDateTimeTest` datapoint is 5-20 minutes and the box has other tenants.
Do not re-land any of it on the strength of unit tests and bt18 alone — both
were green for the reverted version.

## Attempt 3, and the measurement error underneath attempts 1-3

Attempt 3 added the single-pass backend's lazy prologue (`shadow_pushed_any`):
the `get_current_thread` fetch is emitted, then overwritten with `0x90` when the
method turns out never to publish, so offsets recorded during lowering stay
valid. `ZonedDateTimeTest` still timed out.

Three hypotheses, three wrong — which is the signal that the method was wrong,
not just the guesses. Collecting every `ZonedDateTimeTest` datapoint from this
session:

| run | IR work present? | result |
|---|---|---|
| `FINAL-default` (in a 5-class sweep) | **no** | TIMEOUT 900 s |
| `Z-default-gc` (standalone) | **no** | PASS 306 s |
| `MERGED-final` (in a sweep) | **no** | PASS 302 s |
| `IRMAP-final` | yes, claim on | TIMEOUT 1200 s |
| `ZDT-after-withdraw` | yes, claim off | NORESULT 301 s |
| `PUB-validate` | yes, publication | TIMEOUT 1200 s |
| `GATED-zdt` | yes, claim gated off | TIMEOUT 900 s |
| `LAZY-zdt` | yes, lazy prologue | TIMEOUT 900 s |

**This class is bimodal — roughly 300 s or past 900 s — with the IR work absent.**
Two of the three pre-IR runs pass and one times out. So every attribution in
attempts 1-3 rests on comparing one sample against one sample of a bimodal
distribution, which cannot support any of the conclusions drawn from it. The two
reverts may have been unnecessary; equally, publication may be fine or may not
be. Nothing here decides it.

It also has a distinct third outcome — a silent early exit with no `@@RESULT`
after a normal shutdown (`NORESULT` above, and once at 663 s pre-IR) — which is
unexplained and may be the same underlying instability.

**Before any further work on this contract**, fix the measurement:

* characterise the class first — 5+ standalone runs on a quiet host with the IR
  work absent, to get the pass rate and the distribution. If it is genuinely
  bimodal, it cannot be the acceptance gate at all;
* pick a deterministic proxy for the emission cost instead. The cost hypothesis
  is "per method ENTRY", so a microbenchmark over many short-lived compiled
  calls (`CalleeTierUpProbe` shape) measures it directly, in seconds, with
  repeats — rather than inferring it from one 15-minute suite run;
* keep `ASTParserLoadingTest` as the stable large-workload control: it was 138 s
  in every configuration tried, including both publication attempts.

## The measurement, done properly (attempt 4)

`CRATONVM_JIT_IR_RELOC_EMIT=0` disables the whole emission side (safepoint-id
stores, shadow push/reload, prologue thread fetch) on one binary, which is what
attempts 1-3 lacked. Interleaved, five reps per lane, `BinTreesClassic 18`
at `-Xmx512m` — call-heavy and entry-heavy, i.e. the shape the cost hypothesis
predicted would hurt:

| lane | ms | median |
|---|---|---|
| emission OFF | 2566, 2604, 2579, 2604, 2563 | 2579 |
| emission ON  | 2616, 2595, 2614, 2621, 2538 | 2614 |

**~1.4%, ranges fully overlapping** (2563-2604 vs 2538-2621), checksum
`68332206` in both lanes. The emission side is not expensive.

That closes the question attempts 1-3 kept getting wrong: those
`ZonedDateTimeTest` timeouts were the class's own bimodality, not this change.
One lever and ten 2.5-second runs settled what three 15-minute suite runs could
not — the fix was never a better hypothesis, it was a probe that can express the
signal and a control on the same binary.

## Status

Implemented and measured:

* frame side — sp-id slot and store, per-safepoint `OopMapEntry`, `FrameLayout`,
  zeroed `Ref` phi slots;
* emission side — shadow push/reload (RCX temp, `emit_call_return_check` choke
  point, self-recursive route excluded), lazy prologue;
* relocation verified: `cycles=25 coverage_fallbacks=0` on bt18 @512m, checksums
  `68332206` (2g and 512m) and `14985902` (`--nojit` bt16);
* emission cost measured at ~1.4% (above);
* `cargo test -p cratonvm-jit` 1060 lib + every integration target 0 failed.

Still open, and the only thing between this and default-on: the READER-side cost
of asserting coverage. A `true` makes `conservative_roots` run its band scan
instead of taking the early-out, and that cost was never isolated either — the
runs that tried are the same bimodal ones. Measure it the same way this was
measured (`CRATONVM_JIT_IR_RELOC_MAPS=1` vs default, interleaved reps on a
deterministic probe, plus a deep-stack probe since band-scan cost should scale
with live frame count) before flipping the default.

## Reader-side cost: measured, but the probe does not discriminate

Same method as above, `CRATONVM_JIT_IR_RELOC_MAPS=0` vs `=1`, interleaved, five
reps, `BinTreesClassic 18` @512m:

| lane | ms | median |
|---|---|---|
| claim OFF | 2549, 2523, 2520, 2559, 2571 | 2549 |
| claim ON  | 2519, 2543, 2541, 2543, 2563 | 2543 |

No cost — the `ON` lane is nominally 6 ms faster, ranges fully overlapping.

**Do not conclude from this that the assertion is free.** Both lanes report
`cycles=25 coverage_fallbacks=0`, *identically*. If an IR frame were live at any
of those 25 collections, the OFF lane would have had to fall back (its maps say
"not covered") and the ON lane would not. Identical counts mean **no IR frame
was live at collection time in this workload at all** — so the run never
exercised the thing being measured, and the timings above are measuring nothing.

This is the same class of error as attempts 1-3, caught this time before it
became a conclusion: a probe that cannot express the signal produces a confident
null. The tell here was free and worth keeping — the fallback counters
themselves say whether the code path was reached.

### What a discriminating probe needs

* a method the IR tier actually compiles (`ir::ir_compatible`: no `athrow`, no
  `invokedynamic`, within the invoke/field caps) —
* holding a live reference across a call, so the frame has something to publish,
* live on the stack when a young collection runs, and
* deep enough to make band-scan cost visible, since that cost scales with live
  frame count.

Confirm it discriminates BEFORE timing anything: with the claim OFF the run must
show non-zero `coverage_fallbacks`, and with it ON those must drop. If both
lanes agree, the probe is wrong, not the change.

Until such a probe exists the default stays opt-in. The implementation is
complete and sound — publication is real, `cycles=25 coverage_fallbacks=0` and
checksum `68332206` show relocation working through the contract, and the
emission side costs ~1.4%. What is missing is not code, it is a measurement that
can see the reader side.

## A purpose-built probe still does not discriminate — and that is the finding

`bench/IrRelocProbe.java` was written to have all four properties the section
above demands: IR-eligible shape, a live `Ref` held across a recursive call and
read after it, allocation on every frame, and depth so many such frames are live
at once. Two lanes, `CRATONVM_JIT_IR_RELOC_MAPS` 0 vs 1:

    -Xmx256m, depth 40, 40k iters   both lanes: cycles=1  coverage_fallbacks=0
    -Xmx64m,  depth 60, 300k iters  both lanes: cycles=61 coverage_fallbacks=0

61 young collections with a deep recursive stack of exactly the intended shape,
and the claim still makes no difference. With the claim OFF every IR frame's map
says "not covered", so **if an IR frame had been live at any of those 61
collections the OFF lane had to record a fallback.** Zero in both lanes means no
IR frame was live at any of them.

So the reader-side cost cannot be measured this way, and the reason is more
interesting than the number would have been: **IR frames appear not to be live
at young collections in these workloads at all.** Two candidate explanations,
and the next step is to tell them apart — the second would mean the contract is
correct but currently unreachable, which changes what it is worth:

1. `step` is not being IR-compiled (check with `CRATONVM_DBG_JIT_METHOD_STATS`
   and the `IR_LOWER_COMPILES` counter; `ir::ir_compatible` rejects on `athrow`,
   `invokedynamic` and the invoke/field caps, and the optimizing tier has to be
   reached at all);
2. IR-compiled methods are systematically not on the stack when a collection
   happens — e.g. allocation slow paths route through frames the IR tier does
   not produce, so the collection is always initiated below an IR frame rather
   than within one.

Until one of those is settled, flipping the default is unjustifiable in both
directions: there is no evidence it costs anything, and no evidence it buys
anything either. The implementation stands, sound and opt-in; what is missing is
not code and not a timing run, it is knowing whether the path is reachable.

## Resolved: the optimizing tier produces ZERO bodies at runtime

`used_ir_backend` was written and never read outside `cfg(test)`, so "did the
optimizing tier produce any body in this run?" had no runtime answer — which is
precisely what made every probe above unfalsifiable. `CRATONVM_DBG_IR_COMPILES=1`
now prints one line per IR-produced body. On `IrRelocProbe` (`-Xmx64m`, depth 60,
100k iters, 20 young collections):

    IR bodies total: 0
    [GC] moving_young: cycles=20 coverage_fallbacks=0

**Zero.** So explanation (1) is the answer: no IR frame was live at any
collection because the IR backend compiled nothing at all. The contract code has
never executed in any probe in this investigation.

Three earlier numbers must be re-read in that light, and none of them says what
it appeared to:

* the "~1.4% emission cost" measured nothing — there were no IR bodies to emit
  into;
* `cycles=25 coverage_fallbacks=0` on bt18 is the SINGLE-PASS path proving its
  own coverage, not IR;
* the reader-side null is likewise vacuous.

`cargo test -p cratonvm-jit` exercises IR heavily (`ir_vs_singlepass` 89/0), so
the pipeline works when driven directly. What does not happen is the *runtime*
reaching it. Candidates, in order:

1. `ir::ir_compatible` rejecting these methods — `IrRelocProbe::step` contains
   `new` and `invokevirtual`, both admitted in principle, but the caps and the
   `has_athrow` / `indy_ops` rejections are worth printing per candidate;
2. the tiered manager never calling `try_compile` with `optimize = true` in a
   default run, so the gate this session opened is downstream of a decision that
   never selects C2 at all.

**This supersedes the "C2 tier restored" claim** from the gate-scoping work
(`f78b72670`). That change is still right — the gate was guarding an unreachable
hazard — but the ASTParser 376->138 s and Oracle 322->93 s improvements cannot
have come from the optimizing tier if it emits nothing. The other gate scoped in
the same commit, `direct_jit_callee_calls_enabled`, is the likely source and
should be credited (and re-measured) separately.

Next step is (2): instrument the tier decision, not the backend.

## Narrowed: the tier IS requested — `ir_compatible` is what rejects

Follow-up to the zero-bodies result. The suspicion that the runtime never asks
for the optimizing tier is **wrong**; `optimize = true` reaches `try_compile` on
the live dispatch paths:

* `vm/src/jit/helpers.rs:7170` — `try_jit_compile_callee(..., true)`, literal;
* `vm/src/runtime/interpreter/invoke.rs:16445` and `:16598` — both pass a
  literal `true` ("early-compile path is the optimized (C2-equivalent) tier",
  "inline mutator compile path is the optimized (C2-equivalent) tier").

And the admission gate this branch scoped is open by construction:
`moving_young_disables_optimizing_tier()` delegates to
`moving_young_relocates_compiled_frames()` = `moving_young_enabled() &&
JIT_PUBLISHES_RELOCATION_CONTRACT`, and that constant is `false`.

So with `optimize == true` and the moving-young term `false`, the only remaining
conjunct in `try_compile_inner`'s admission is **`ir::ir_compatible(&scan)`**
(and the per-method caps behind it). That is where the zero comes from.

Next step, and it is small: `ir_compatible` currently answers `bool`, so a
rejection is invisible. Give it a reason — an enum or a `tracing::debug!` per
rejected conjunct (`has_athrow`, `!indy_ops.is_empty()`, `invoke_ops.len() >
IR_MAX_INVOKES`, the field/bytecode-size caps) behind the existing
`CRATONVM_DBG_JITC`, then run `IrRelocProbe` and read which one fires for
`step`. Only after that does it make sense to ask whether the rejection is
correct, whether the cap should move, or whether the probe should be reshaped.

Note the shape of the mistake being avoided here: "the tier is off" was inferred
twice from an absence (no IR bodies, then no live IR frames) without checking
which conjunct produced it. The absence is the same in all cases; only the
reason distinguishes them, and nothing currently reports the reason.

## Retraction: the "ir_compatible never called" result was a harness error

An attempt to add per-conjunct rejection reporting to `ir_compatible` produced
"zero refusals AND zero IR bodies", which was read as proof that the `&&` chain
short-circuits before `ir_compatible` — i.e. that `optimize` is false on the
path that compiles hot methods.

**That reading is void.** The patch never reached the tree that was built. The
script targeted `C:\craton\CratonVM\jit\src\ir.rs` (backslashes) and the `sed`
meant to redirect it to the task worktree matched on forward slashes, so it
silently did nothing. The diagnostic was applied to the dev worktree three times
over and to the task worktree never; the binary under test contained no
reporting at all, so "zero refusals" only means "nothing was instrumented".

Both trees have been reverted. Nothing is known about which conjunct fires.

The narrowing in the section above still stands on its own evidence — three call
sites pass `optimize = true` literally, and the moving-young term is `false` by
construction — so `ir_compatible` remains the prime suspect. It is just not yet
demonstrated.

**When redoing this:** apply the patch, then *verify it is in the tree you are
about to build* (`grep -c ir_reject jit/src/ir.rs`) before building, and confirm
the built binary emits at least one line on a method you know is refused. Three
separate conclusions in this investigation have now come from instrumentation
that was not actually running — zero fallbacks with no live IR frame, zero IR
bodies, and now zero refusals. Absence of output is not evidence until the
output path is known to work.

## Redone with verification: `ir_compatible` is NOT reached

The reason reporting was re-applied, and this time verified in the tree that was
actually built (`grep -c ir_reject jit/src/ir.rs` → 10 = 9 sites + the helper;
dev confirmed at 0; binary rebuilt after). On `BinTreesClassic 16` with
`CRATONVM_DBG_IR_COMPILES=1`:

    refusals seen: 0
    IR bodies:     0

Both zero, with the reporting code demonstrably compiled in. That combination is
what makes it informative: had `ir_compatible` been called it must either return
`true` — producing an IR body — or `false`, which now logs. Neither occurred, so
**`ir_compatible` is never reached**; the `&&` chain in `try_compile_inner`
short-circuits before it.

Caveat worth stating rather than glossing: this is inference from a double
absence, and the session's own history is three wrong conclusions drawn from
absences. There is still no *positive* control — no observation of the
diagnostic firing on a case known to be refused. The inference is sound only
because the refusal branch and the success branch have distinct, mutually
exclusive observable outcomes and neither appeared.

That leaves the two conjuncts ahead of it in
`if optimize && !moving_young_disables_optimizing_tier() && ir::ir_compatible(..)`:

* `optimize` false on whatever path compiles these methods. Three call sites
  pass a literal `true` (`helpers.rs:7170`, `invoke.rs:16445`, `:16598`), but
  those are the inline-dispatch and early-compile routes; the background
  tier-up route takes `optimize` as a parameter (`try_jit_compile_callee`) and
  its value at the hot-method path was never traced to a literal.
* `moving_young_disables_optimizing_tier()` true. It should be `false` by
  construction, but it is `pub` and its `tracing::warn!` fires only on the
  first call — easily missed.

**Next step, and it is one line each:** log both at the top of
`try_compile_inner`, behind `CRATONVM_DBG_IR_COMPILES`. That is a positive
control as well as the answer — if neither line appears, `try_compile_inner`
itself is not on the path, which would be a third possibility nobody has
considered.
