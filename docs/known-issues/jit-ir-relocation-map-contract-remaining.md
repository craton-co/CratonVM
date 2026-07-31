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

## ANSWERED: two causes, and `ir_compatible` is not either of them

A probe at the top of `try_compile_inner` (verified in the built tree first)
printing `optimize` and `moving_young_disables_optimizing_tier()`.
`BinTreesClassic 16`, `CRATONVM_DBG_IR_COMPILES=1`:

    2x  optimize=false  moving_young_disables_tier=false
    1x  optimize=true   moving_young_disables_tier=false
    refusals: 0   IR bodies: 0

Three findings, and the first two retract earlier sections:

1. **`moving_young_disables_tier=false` on every call.** The gate this branch
   scoped is genuinely open. That part works.
2. **Most compiles ask for `optimize=false`.** Two of the three calls are the
   ordinary tier-up route requesting the single-pass backend *by design*. So
   "the optimizing tier never runs" is substantially just "it is rarely
   requested" — not a bug in the admission chain at all, and not something the
   relocation contract can affect.
3. **The one `optimize=true` call produced neither a refusal nor a body.**
   Since `ir_reject` now logs every `ir_compatible` refusal, and no refusal was
   logged, `ir_compatible` **passed** — and the compile was then rejected by a
   conjunct AFTER it in the same `if`. The chain continues past `ir_compatible`
   into the STUB-S8 exception-table condition and the rest; one of those is the
   real refusal, and none of them are instrumented.

So the suspect list has moved twice — first to `ir_compatible`, now past it —
and each move came from adding one observation rather than one hypothesis.

### Next

Instrument the conjuncts AFTER `ir_compatible` in `try_compile_inner`'s `if`,
the same way (`ir_reject`-style, one call per condition). That names the actual
refusal for the `optimize=true` case in a single run.

Then, separately, decide whether finding (2) matters: if the tier-up path is
meant to request C2 for hot methods and does not, that is a much larger
throughput question than the relocation contract, and it belongs in the
tiered-manager work (`docs/feature-designs/wire-tiered-manager.md`), not here.
The relocation contract is ready for whichever methods do reach the IR backend.

### The post-`ir_compatible` conjuncts, narrowed by inspection

The chain continues (jit/src/lib.rs, after `ir::ir_compatible(&scan)`):

```
&& !(exc_table_c2_disabled() && !cached.exception_table.is_empty())
&& !precise_exception_frames
&& ((!method_uses_category2(..) && !method_uses_fp(..)) || <long/FP clauses>)
… and more past that
```

Two are ruled out by inspection for the observed `optimize=true` case
(`BinTreesClassic.itemCheck`):

* `exc_table_c2_disabled()` reads `CRATONVM_JIT_NO_EXC_TABLE_C2`, which is
  opt-in and unset, so that term is `true`;
* `precise_exception_frames` is set only where RBC.6 fires — a handler reading a
  local it never wrote. `itemCheck` has no `try`/`catch` at all, so it cannot.

And `itemCheck(TreeNode) -> int` is category-2-free and FP-free, so the third
clause should hold too. **So the refusal is in a conjunct further down than the
ones read here**, and inspection has run out — the remaining terms need the same
one-call-per-condition instrumentation, not more reading.

That is the whole of the remaining work on this thread, and it is mechanical:
add an `ir_reject`-style call to each conjunct from `exc_table_c2_disabled`
onward, rebuild (verifying the patch is in the tree first — see the retraction
above), and run `BinTreesClassic 16` once. The refusal names itself.

Worth keeping in view while doing it: finding (2) above means this only ever
affects the *rare* `optimize=true` compile. Even fully fixed, the contract
covers whichever methods reach the IR backend — which today is close to none,
because the tier-up path requests C1 by design. **Whether that is right is the
larger and more valuable question**, and it lives in the tiered-manager work,
not here.

### Resolved by construction: the admission chain PASSES; the pipeline bails inside

Reading the condition to its end (`jit/src/lib.rs`, the `if` closes at the
`{` before `let num_params = prologue_param_slots;`), the full chain is:

```
if optimize
    && !moving_young_disables_optimizing_tier()
    && ir::ir_compatible(&scan)
    && !(exc_table_c2_disabled() && !cached.exception_table.is_empty())
    && !precise_exception_frames
    && ((!cat2 && !fp) || (ir_emit_long && !fp) || (ir_emit_fp && fp_in_body))
```

For the observed `optimize=true` case, `BinTreesClassic.itemCheck`:

| conjunct | value | why |
|---|---|---|
| `optimize` | true | observed |
| `!moving_young_disables_optimizing_tier()` | true | observed (`=false`) |
| `ir::ir_compatible(&scan)` | true | no refusal logged, and every refusal now logs |
| `!(exc_table_c2_disabled() && …)` | true | that variable is opt-in and unset |
| `!precise_exception_frames` | true | only set where RBC.6 fires; `itemCheck` has no handler |
| `(!cat2 && !fp)` | true | `itemCheck(TreeNode) -> int` is category-2-free and FP-free |

**All six pass.** So the block IS entered and the IR pipeline is run — and no
body results, which means it bails *inside* build → schedule → lower and returns
`None`, after which the caller silently falls through to single-pass.

`lower_inner` alone has three such bails, each already commented as a
"soundness bail": `unallocated_slot_use` (a node emitted with no frame slot),
`buf.overflowed()` (an under-estimated buffer), and the earlier `ir_compatible`
paths. `IrBuilder::build` can return `None` too. **None of them log.**

So the search is over and the target is named: it is not the admission chain at
all, it is a silent `None` from the IR pipeline itself. Instrument those bail
sites — they are few, all already marked in comments — and one run names it.

This also explains, without any further measurement, why every probe in this
investigation saw zero IR bodies while `cargo test -p cratonvm-jit` exercises IR
heavily: the tests call `lower()` on graphs they construct directly, bypassing
the build-from-bytecode step where the production bail happens.

## FOUND: `IrBuilder::build` returns `None` — the pipeline bails at stage one

Instrumented the three silent bails (`IrBuilder::build`, `lower_inner`'s
`unallocated_slot_use` and `buf.overflowed`), verified present in the built tree,
one run of `BinTreesClassic 16`:

    [ir] try_compile_inner BinTreesClassic.itemCheck optimize=true moving_young_disables_tier=false
    1x  BAIL IrBuilder::build returned None for BinTreesClassic.itemCheck
    IR bodies: 0

So the complete chain, end to end, is now known:

1. **Most compiles never ask for C2** — the tier-up path passes
   `optimize=false` by design. Nothing about the IR backend is involved.
2. **The rare `optimize=true` compile passes the entire admission chain** —
   including `ir_compatible`, and including the moving-young gate this branch
   scoped, which is `false` exactly as intended.
3. **`IrBuilder::build` then returns `None`** — graph construction from bytecode
   refuses the method, before scheduling or lowering is ever reached.

`itemCheck` is `static int itemCheck(TreeNode) { if (t.left == null) return
t.item; return t.item + itemCheck(t.left) - itemCheck(t.right); }` — recursion,
a null test, two `getfield`s. If the builder cannot construct a graph for that,
the population it *can* build for is very small, which is consistent with every
observation in this investigation.

### What this means for the relocation contract

It is not the blocker and never was. The contract is implemented, sound and
fail-closed; it will cover whichever methods reach the IR backend. What is
missing is upstream of it by two stages: methods rarely request C2, and the
builder refuses the ones that do.

### Next, and it is a different piece of work

`IrBuilder::build` returns a bare `Option`, so its refusal is as unnamed as
`ir_compatible`'s was before this session. Give it a reason the same way — the
`_ => return None` in its opcode loop is the obvious first suspect — and run the
same probe. That names the unsupported construct in one run.

Then the real question is a scoping one, not a debugging one: is the IR builder
*meant* to handle ordinary recursive field-accessing methods? If yes this is a
gap worth closing and the optimizing tier is largely inert today. If no, the
tier is narrower than the surrounding documentation implies, and several open
throughput documents that assume C2 participation need re-reading — including
this branch's own retracted "C2 tier restored" claim.

## NAMED: `newarray` (0xbc) is unsupported, and admission disagrees with the builder

Naming the builder's catch-all refusal the same way:

    [ir] BAIL IrBuilder::build unsupported opcode 0xbc at pc 2
    [ir] BAIL IrBuilder::build returned None for IrRelocProbe.step

`0xbc` is `newarray` — **primitive array allocation**. `IrRelocProbe.step`
opens with `new int[6]`, so the builder refuses it at pc 2, before anything else
in the method is even looked at.

**The admission predicate and the builder disagree.** `ir_compatible` carries
`IR_MAX_ALLOCATIONS` budgets for `scan.new_ops` and `scan.anewarray_ops` — i.e.
it is written as though allocation is supported and merely capped — and it
admits the method. `IrBuilder::build` then refuses it on the first primitive
array allocation. Every such method pays a full admission pass, a graph-build
attempt, and a silent fallback to single-pass.

That is the whole reason this took a session to find: two components with
different ideas of what the IR tier accepts, and neither of them said so out
loud.

`BinTreesClassic.itemCheck` bails from a DIFFERENT path — it printed the
`returned None` line but no opcode line, so its refusal is one of the builder's
other ~19 `return None` sites, not the opcode catch-all. Those remain unnamed;
the same one-line treatment will name them.

### The decision this surfaces

`newarray` is not exotic. If the optimizing tier cannot build a graph for a
method that allocates a primitive array, its addressable population is very
small, and that is a scoping question rather than a bug:

* if the IR builder is *meant* to handle allocation, `newarray` is a gap worth
  closing and `ir_compatible`'s allocation budgets are currently lying;
* if it is not, then `ir_compatible` should refuse these methods up front —
  cheaply, and visibly — instead of admitting them into a build that cannot
  succeed, and the surrounding documentation that treats C2 as a general tier
  needs correcting.

Either way the fix belongs with the IR builder's owners. The relocation contract
is downstream of all of it and is ready.

## Verified: `Op::NewArray` is dead scaffolding — array allocation was never wired in

Checking whether `newarray` support might be a small delta on existing
`anewarray` machinery: it is not. `ir::Op::NewArray { element_type }` is
**declared and never constructed, and never lowered** — no `Op::NewArray`
appears anywhere in `ir.rs`'s builder or in `ir_lower.rs`. So reference-array
allocation is not supported either; the variant is an unimplemented stub.

That settles the scoping question raised above, and settles it against
`ir_compatible`: its `IR_MAX_ALLOCATIONS` budgets for `new_ops` and
`anewarray_ops` gate a capability the builder does not have at all. They are not
"capped", they are absent.

So closing this is a genuine feature: construct the node in `IrBuilder::build`,
lower it in `ir_lower` to the allocation helper, and — because allocation is a
GC-capable point — give it a safepoint, which means routing it through the very
`emit_safepoint_map` / `emit_shadow_push` machinery this branch added. The
relocation contract is a prerequisite for that work, not a consequence of it.

**Deliberately not attempted here.** A new allocation path in a JIT, with a
GC-capable safepoint, is not something to write against an exhausted context and
validate with a checksum. It belongs with the IR builder's owners, and it wants
the same acceptance the rest of this file asks for: instrument first, confirm the
diagnostic fires, then measure.

### Summary of the whole chain, for whoever picks this up

| stage | state |
|---|---|
| moving-young gate on the C2 tier | scoped; verified open (`moving_young_disables_tier=false`) |
| tier-up requesting C2 | mostly `optimize=false` **by design** |
| `ir_compatible` | passes (and now names its refusals) |
| `IrBuilder::build` | **refuses `newarray` 0xbc**; `Op::NewArray` unimplemented |
| IR relocation map contract | implemented, sound, fail-closed, merged, unexercised |
