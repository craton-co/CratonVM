# The differential harness passed the VM context to a body that did not want one — and only an optimization doing its job made it visible

## Status

**FIXED, 2026-08-28. The compiler was never wrong.**
`jit/tests/ir_vs_singlepass.rs` is **142/142 with `CRATONVM_SCALAR_DEOPT=1` and
142/142 without it**, and `cargo test -p cratonvm-jit --lib` now gives the same
result in both arms.

The failure was in the harness's calling convention, not in escape analysis, not
in `plan_scalar_replacement`, and not in the flag. **The narrowing this page's
predecessor applied to `scalar-deopt-gauntlet-soak-20260827.md` is withdrawn:
that record's "which is also why the flag is safe" was right, and the objection
raised against it was not.**

## The defect

`CompiledMethod::try_call_with_context` passes the VM context in `ABI[0]`. That
is not an extra argument the callee may ignore — a body compiled *without* a
context reads its **first Java argument** from that same register. Supplying one
anyway shifts every argument by one position.

So `int f(int n)` returned the context pointer where `n` belonged:

```
assertion `left == right` failed: the field written is the field read back
  left: 969312034704        <- the address of the harness's dummy VM
 right: 11
```

The VM has always branched on `CompiledMethod::needs_context()` before choosing
between `try_call` and `try_call_with_context`
(`vm/src/jit/helpers.rs`, `jit_bridge.rs`). The harness called one way for a
whole corpus of 33 sites.

## Why it took a flag to expose it, and why that is the interesting part

`needs_context` is **an output of optimization, not a property of the source**.
`scan_frame_needs` sets it from what is still in the graph: an `Op::New`, an
`Op::Call`, a reference store, a helper-served field access. For

```java
int f(int n) { Corpus c = new Corpus(); c.f0 = n; return c.f0; }
```

it is true only because of the `new`. Escape analysis had always *offered* that
allocation as scalar-replaceable; `CRATONVM_SCALAR_DEOPT` is what lets the offer
be acted on. Acting on it removes the last context-needing node, `needs_context`
flips to false, the body's ABI changes — and the harness kept calling the old
way.

**An optimization doing exactly its job silently changed the calling convention
under a caller that had assumed it was fixed.** The symptom then appeared inside
the optimization, which is where two sessions went looking.

The IR was correct throughout and said so plainly once dumped
(`CRATONVM_DBG_IR_GRAPH=1`, added for this):

```
[ir-graph] Corpus.f(I)I — 9 node(s), entry=0 exit=8
[ir-graph]     3: Param(0) : Int <- [0]
[ir-graph]     8: Return : Void <- [1, 3]      <- returns n. Correct.
```

`Return` reads `Param(0)`. The graph was right; the value arriving in `Param(0)`'s
register was not.

## The fix

`try_call_with_context` routes to `try_call` when `needs_context()` is false.
Its doc comment already said "call a compiled method **that needs VM context**",
so this makes the function enforce the contract it documented rather than
trusting every caller to have checked. One place, all 33 harness sites, and any
future one.

Routed rather than refused: every caller wants "invoke this method", and there
is no caller for whom forcing the context ABI onto a body that did not ask for
one is the right thing.

## Three tests that asserted a flag's default as an invariant

Once the ABI was right, four assertions still failed with the flag on. None was
a defect; each pinned the **flag-off** behaviour without saying so:

| test | asserted | true with the flag on |
|---|---|---|
| `ir_elidable_trivial_init_on_fresh_new_is_still_elided` | the allocation count is `i + 1` | 0 — the allocation is really gone |
| `ir_new_scalar_replaces_end_to_end` | a snapshot names the `Op::New`, which is retained | it is elided |
| `ir_new_scalar_replaces_through_astore_local` | the `Op::New` is retained | it is elided |
| `ea_refuses_to_elide_an_allocation_a_safepoint_names` | the allocation survives | it does not |

Each now asserts the correct answer **for both arms** rather than being relaxed
to accept either — the count is the only evidence in those tests that the
elision happened at all, and an assertion that takes either answer would pass on
a build where the flag had silently stopped working.

The first of these had predicted its own obsolescence: *"If this assertion ever
fails because arm 1's count went to ZERO, that is an improvement, not a
regression."* It did, and it is. The residual it pointed at — escape analysis
offering a replacement that the emitted body ignored — is what the flag closes.

Note for the arm-on case in `ir_new_scalar_replaces_end_to_end`: a snapshot slot
*does* still name the now-dead node, and that is the design rather than a leak —
`build_scalar_replacement_map` describes it and
`ir_lower::frame_value_for_object` resolves the slot to a
`FrameValue::VirtualObject`. That pairing is checked by `ir_lower`'s own
`test_scalar_deopt_emits_virtual_object` and end-to-end by
`probes/ScalarDeoptProbe.java`.

## Also fixed on the way through

`jit/src/x64/tests.rs` still called `intern_inline_invoke_targets` with a
`Vec<usize>` after `b56bb79b9` gave the pin an epoch (`Vec<(usize, u64)>`).
**The entire `cratonvm-jit` lib test binary did not compile on dev**, so all
2 114 tests in it were unrunnable — not failing, unrunnable, which is quieter.

## What this cost, and the general lesson

Two sessions treated a wrong VALUE as a miscompile in the optimization that
exposed it. The tell that should have redirected sooner was already in hand: the
same source shape compiled the ordinary way was **correct**
(`probes/ScalarDeoptProbe.java`, `rescued=6` and right answers). Two environments
disagreeing about one graph means one of them is not running the graph you think
it is — and the cheap next step is to compare the ENVIRONMENTS, not to re-read
the optimization.

**A calling convention that depends on an optimization's outcome is a contract
between the compiler and every caller, and it needs a single enforcement point.**
It now has one.

## Reproducing

```bash
CRATONVM_SCALAR_DEOPT=1 cargo test -p cratonvm-jit --test ir_vs_singlepass   # 142/142
cargo test -p cratonvm-jit --test ir_vs_singlepass                           # 142/142

# the graph dump that settled it
CRATONVM_DBG_IR_GRAPH=1 CRATONVM_SCALAR_DEOPT=1 cargo test -p cratonvm-jit \
    --test ir_vs_singlepass ir_elidable_trivial_init_on_fresh_new_is_still_elided \
    -- --nocapture
```
