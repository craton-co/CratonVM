# `CRATONVM_SCALAR_DEOPT` fails the IR-vs-single-pass differential, and the same shape passes in a real program

## Status

**Open, and it blocks the flag's default-on flip.** `jit/tests/ir_vs_singlepass.rs::ir_elidable_trivial_init_on_fresh_new_is_still_elided`
passes with the flag off and FAILS with it on, returning a heap-address-shaped
value where an `int` belongs:

```
assertion `left == right` failed: the field written is the field read back
  left: 673487773456        <- varies per run; an address
 right: 11
```

The method is `int f(int n) { Corpus c = new Corpus(); c.f0 = n; return c.f0; }`.

Reproduced on **pristine `origin/dev`** (`fbeffaf79`), with this branch's
instrumentation reverted, so it is not an artifact of the census added
alongside it.

**And the same source shape, compiled the ordinary way in a real program, is
correct.** `probes/ScalarDeoptProbe.java` runs write-then-read, a two-field
object, and read-before-write for 400 000 iterations with the flag on; all
three return the right answers and the engagement census says the elision
really happened:

```
[scalar-deopt] census: rescued=6 blocked=0 materialized=0
```

So this is a **red pre-flip gate with an unexplained disagreement underneath
it**, not a demonstrated production miscompile. Both halves are facts; which one
generalises is the open question, and it has to be answered before the flip
because the differential harness is the tree's designated gate for exactly this
class of change (`docs/feature-designs/activate-ir-optimizer.md`: "Every
widening shipped behind its own flag with a differential test").

## What is and is not the trigger

The elision itself is, and nothing else. Same flag, same test, three arms:

| arms | `elide_alloc` | result |
|---|---|---|
| (none) | false | PASS — returns 11 |
| `CRATONVM_SCALAR_DEOPT=1` | **true** | **FAIL — returns an address** |
| `CRATONVM_SCALAR_DEOPT=1 CRATONVM_DEOPT_REAL=0` | false | PASS — returns 11 |

The third arm is the control that isolates it: the flag is still set, but
`deopt_descriptor_available` is `scalar_deopt_enabled() && deopt_real_enabled()`,
so closing the second half closes the gate and the allocation is kept. Nothing
else about the compile changes.

Escape analysis offers the replacement in **both** arms — `CRATONVM_DBG_SCALAR_NEW`
reports `scalar-replaced 1/1 alloc(s)` with the flag off too, and the loads are
forwarded either way. The flag decides only whether the `Op::New` and its stores
are then killed.

> Note for anyone re-running this: `cargo test` captures the output of PASSING
> tests, so the flag-off arm looks like it prints no `scalar-replaced` line at
> all. It does — pass `-- --nocapture`. Reading that silence as "the analysis
> did not run without the flag" sends you looking for a gate that is not there.

## What the test's own comment predicted, and why this is not it

The test anticipates being broken by an improvement:

> If this assertion ever fails because arm 1's count went to ZERO, that is an
> improvement, not a regression: change it to 0 and delete that residual.

That is about the ALLOCATION COUNT assertion. The assertion that fails is the
one above it — `r == n`, the returned value — so this is not the predicted
improvement and must not be resolved by relaxing the count.

The residual the comment refers to (EA offers the replacement and the emitted
body allocates anyway, `cov-04-the-invoke-arms-RETIRED-20260803.md`) is
nonetheless the thing this flag closes: with the flag on, the offer is finally
acted on. The residual was masking whatever this is.

## Why a green differential run says nothing without the census

Across the whole 142-test corpus with the flag on, **exactly one** allocation is
elided — the one in the failing test:

```
$ CRATONVM_SCALAR_DEOPT=1 CRATONVM_DBG_SCALAR_NEW=1 cargo test -p cratonvm-jit --test ir_vs_singlepass
... 1 REPLACED, in Corpus.f(I)I
test result: FAILED. 141 passed; 1 failed
```

The other 141 passes are not evidence about this flag: it never engaged in them.
Its engagement rate in this corpus is one, and its failure rate on that one is
one. That is why `cratonvm_types::scalar_deopt_census` exists — see
`docs/jit/scalar-deopt-soak-20260827.md`.

## Reproducing

```bash
# The failure (pristine dev reproduces it too):
CRATONVM_SCALAR_DEOPT=1 cargo test -p cratonvm-jit --test ir_vs_singlepass \
    ir_elidable_trivial_init_on_fresh_new_is_still_elided

# The control that isolates the elision:
CRATONVM_SCALAR_DEOPT=1 CRATONVM_DEOPT_REAL=0 cargo test -p cratonvm-jit \
    --test ir_vs_singlepass ir_elidable_trivial_init_on_fresh_new_is_still_elided

# The same shape end-to-end, which PASSES. `CRATONVM_JIT_C2_ALLOC_UPGRADE=1` is
# load-bearing: without it `c2_upgrade_would_engage` keeps an allocation-bearing
# method out of C2 entirely, escape analysis never runs on it, and the census
# prints nothing at all.
javac -d probes/sdout probes/ScalarDeoptProbe.java
CRATONVM_JIT_C2_ALLOC_UPGRADE=1 CRATONVM_SCALAR_DEOPT=1 \
  cratonvm --java-home <jdk25> -cp probes/sdout ScalarDeoptProbe
```

## Where to look

`plan_scalar_replacement` in `jit/src/lib.rs`. `elide_alloc` is the one bit the
flag moves; the load forwarding (`EaVictimKind::Forwarded`) is identical in both
arms and is evidently correct, since the flag-off arm returns the right value
with the same forwarding applied. So the fault is in what killing the `Op::New`
and its stores does — `EaVictimKind::Eliminated`, the memory-chain splice, or a
consumer left naming the dead allocation.

The returned value being the allocation's own address is the clue worth starting
from: something the `Return` reaches resolves to the `Op::New` rather than to
the forwarded field value. Whether the real VM avoids it because its graph
differs (real helpers, a real `<init>` chain) or because it never reaches the
same node shape is the first thing to establish — the two environments disagree
and only one of them can be right about this graph.
