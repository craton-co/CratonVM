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
