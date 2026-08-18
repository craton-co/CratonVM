# A lambda SAM call bypassed the cached invoke path — RETIRED 2026-08-18

| | |
|---|---|
| **Status** | RETIRED — both defects it named are fixed, shipped and pinned; the residual it leaves has a mechanism, a count and a named fix class |
| **Opened** | 2026-08-17 as `known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md`; §5 added the same day |
| **Closed by** | `fix/lambda-sam-jit-tierup-20260817` |
| **Measured effect** | **2.3x–2.8x** on every lambda row of `probes/SamHotLoopProbe.java` — `ifaceLambda` 434 → 192 ns/op, method reference 435 → 195, capturing 552 → 200 — same binary, ABBA, six runs an arm, non-overlapping ranges, with the named-class control unmoved |
| **Kill switches** | `CRATONVM_JIT_LAMBDA_TIERUP=0` (both halves), `CRATONVM_JIT_LAMBDA_SITE=0` (the compiled-caller half only) |

The page asked for one thing in its §4 — *"giving lambda call sites a cached
invoke target of their own"* — and reported in §5 that the attempt at the other
half crashed the VM. Both are now done. What made them doable was not a new
idea; it was an instrument that could say which of two indistinguishable things
was happening.

## 1. The measurement that named the defect

§4 reasoned from a flat profile: `try_lambda_dispatch` reached
`invoke_on_class_shared_inner`, which allocates, pins, takes the
`lambda_proxies` lock a third time and resolves by name — while an ordinary
`invokeinterface` "goes through the resolved-callsite cache and the JIT's
monomorphic inline cache, which is why it is 9 ns."

That is right about the destination and wrong about the size, and the way to
tell is `CRATONVM_DBG=mic-prof` on one shape at a time
(`probes/SamHotLoopProbe.java`, written for this page because
`SamDispatchDecompositionProbe` runs eight rows in one process and its boxing
row owns any profile taken over the whole thing):

| receiver | ns/op | `mic_calls` | `hit_entry` | `lambda` |
|---|---:|---:|---:|---:|
| named class | 11.7 | **1** | 0 | 0 |
| lambda | 352.5 | **2 197 000** | 0 | 2 197 000 |

**One helper entry per 2.2 million dispatches, against one per dispatch.** For
a named class, `jit_invoke_virtual_mic` runs ONCE per call site: it resolves the
callee, fills the monomorphic inline cache, and from then on the cascade emitted
in `jit/src/x64.rs` calls the callee from machine code and never returns to
Rust. A lambda receiver is caught by an arm that sits BEFORE that cache and
returns from inside it, so the slot is never populated and never probed —
`hit_entry=0`, `miss=0`, ~819 cycles a call, forever.

So the defect was not "the generic dispatch machinery is expensive". It was
**one early `return` standing between a SAM call site and the same inline cache
every other interface call site gets** — and no amount of memoizing inside that
machinery could have reached it, which is exactly why §3's `OnceLock<ClassId>`
memo moved nothing.

## 2. What was built

Two halves, because a SAM call has two kinds of caller and they are served by
different code.

**The compiled caller** — `jit::helpers::try_lambda_site_direct_call`. The
inline cache cannot itself hold a lambda: its cascade passes the caller's own
argument registers straight through, and a SAM call's registers are not the
impl method's (the proxy receiver has to go, the captured values have to
arrive). So the call site's target is cached one level out, in Rust, per proxy
`ClassId`, in a thread-local. Spending it is: read the captures out of the
proxy's fields, put the SAM's already-decoded raw arguments after them, call the
compiled impl through `try_call_compiled_entry_reentrant_owned` — the same
primitive the monomorphic hit path uses, with the same `i64::MIN` deopt
handling and the same "an escaping exception is left in `jit_pending_exception`
for the compiled caller's own post-invoke check".

Everything that makes a lambda dispatch complicated is decided ONCE, when the
site is built, and a shape that needs any of it is cached as ineligible and
never asked again. The eligibility rule is that coercion must be provably the
identity — which `coerce_arg` is for equal tokens *and* for two reference
tokens, so `Function<Integer,Integer>` (the generic shape, where javac's bridge
would have inserted a `checkcast`) qualifies with the cast replayed per call and
a cast that would FAIL simply declining to the generic path, which then throws
the `ClassCastException` with the message it has always built.

**The interpreted caller** — §5's missing tier-up, plus the primitive its first
attempt lacked. The warmup counter is the same shape as the invokestatic and
invokevirtual twins. Entering the compiled body is NOT: `execute_jit_call_decoded`
pushes its result onto a caller's operand stack and, on a routed exception or a
precise-resume deopt, pushes an interpreter FRAME and returns
`CachedCallResult::FramePushed`, meaning "the stepping loop will run it". A
dispatch helper is not that loop. §5.3's crash — a `usize::MAX` operand-stack
underflow, thousands of calls later, inside an unrelated interpreted run of the
same body — was an orphaned frame from exactly that mismatch, and §5.4 named the
two ways out. This took (b): `jit_bridge::execute_jit_call_oneshot`, which keeps
that function's run/signal/deopt logic verbatim and differs in the two places
that matter — a normal return is CONVERTED to a `Value` and returned rather than
pushed, and a sink that materialises a frame has that frame RUN TO COMPLETION
here (`run_pushed_frame_to_completion`, split out of `execute_prebuilt_frame`),
so the handler or resumed body finishes as part of the call and nothing is left
on `thread.frames`.

Two smaller things fell out. `CRATONVM_BG_COMPILE=0` — the documented opt-out
that restores inline compilation — did nothing on the lambda path in the first
cut, so the off-switch would have silently disabled the feature rather than
changed how it compiles; it now compiles inline like the twins. And the TDigest
`get(I)D` special case in `lambda.rs`, which entered a compiled body directly
and ignored every out-of-band signal it might raise, is gone: the general path
subsumes it and drains them.

## 3. The numbers, beside the count of calls that produced them

Same binary, kill switch, ABBA, three rounds of A-B-B-A, six runs an arm, Azure
8-core under other sessions' load (which is why the absolute numbers are above
the page's original 355 ns; the ratios are the statement).

| row | tier-up ON | OFF | |
|---|---:|---:|---|
| named class (control) | 12.6 | 11.8 | unmoved, as it must be |
| `ifaceLambda` | **191.7** | 434.4 | **2.27x** |
| method reference | **195.3** | 435.1 | 2.23x |
| capturing | **199.6** | 552.0 | **2.77x** |

Ranges do not overlap on any lambda row: ON `[183.9 … 207.2]` against OFF
`[414.0 … 466.0]` for `ifaceLambda`.

And the engagement, printed beside them (`CRATONVM_DBG=lambda-jit`), because a
flat A/B on this path cannot tell "the direct call did not help" from "no direct
call ever happened":

```
[LAMBDA-JIT] eligible=2999 compiled_hits=2103 fast_returns=2103 declines=0
             site_calls=1100000 site_direct=1100000 site_no_code=0
             site_refused=0 site_deopted=0 site_arity=0
```

1 100 000 of 1 100 000 — every dispatch after warmup took the direct arm, none
refused, none declined.

## 4. The residual, and why it is not this page's

The gap to the named-class row is 15x, down from 34x. Closing it needs what
section 1 says it needs: a lambda receiver that the inline cache can hold. That
means a per-proxy ADAPTER — a small piece of generated code that shifts the
captured values into the argument registers and jumps to the impl — so the MIC
can cache it like any other callee and the cascade can call it from machine code
without re-entering Rust at all. That is JIT codegen work, deliberately not
attempted here, and it is the honest successor to this page.

**What is emphatically NOT the successor is the workload this page was filed
from.** `residual-seven-after-the-afc-fix-20260817.md` put ~55% of
`MultithreadedInsertionTest`'s samples in `CompletableFuture` composition, and
this page inherited the inference that composition is slow because SAM dispatch
is slow. It is not. `probes/LambdaCompositionProbe.java` — `thenApply` /
`thenCompose` chains, the real shape — measures:

| | CratonVM | HotSpot | |
|---|---:|---:|---|
| `thenApply` | 12 286 ns/stage | 82.5 | 149x |
| `thenCompose` | 11 943 ns/stage | 113.4 | 105x |

A SAM dispatch is ~190 ns even before this fix's 2.3x, so it cannot be more than
a low single-digit percentage of 12 µs — and the A/B agrees, moving those rows
−1.1% and −3.3%. Two further facts name where the time is instead:

* `site_calls=0` on that probe. The composition path's SAM calls come from
  INTERPRETED callers, so the compiled-caller half never runs; only the
  interpreted half applies, and it is the smaller of the two.
* `--nojit` measures 11 470 / 17 443 ns/stage — **the same**. A workload the JIT
  does not change is not a workload whose cost is dispatch.

So the hibernate-reactive composition residual needs its own investigation,
starting from a profile of `LambdaCompositionProbe` (flat: the interpreter loop
at 7%, the native registry's three lookup functions at ~5.8%, allocation ~3%),
and it should not be filed as a lambda problem.

## 5. What pins it

`vm/tests/lambda_jit_tierup_tests.rs` and `lambda_jit_oneshot_tests.rs` — the
same twelve golden checksums from a real JDK, run against each half — plus
`lambda_jit_engagement_tests.rs` and its `_oneshot_` mirror, which assert the
half under test actually served the calls.

That last pair exists because the first version of this suite was worthless and
said nothing about it. Twelve tests, all green, at 4 000 iterations an arm —
and with a deliberate off-by-one planted in the one-shot's return conversion and
its implicit-NPE drain deleted outright, **eleven of the twelve still passed**.
They had computed their answers in the interpreter and agreed with HotSpot about
a path they never took. Three things fixed that, and all three were needed:

1. **200 000 iterations**, not 4 000, so an asynchronous compile cannot outlast
   the loop.
2. **`CRATONVM_BG_COMPILE=0`**, so the body compiles on the mutator at the
   threshold and "compiled by iteration ~501" is a fact rather than a race.
3. **A one-line static `step` hop** that every SAM call goes through, because
   which half serves a call is decided by whether the CALLING frame is compiled
   — and a loop sitting directly in a test method leaves that to an OSR race the
   test cannot see.

Then each break fails its own suite, and the engagement tests report
`site_direct=399 500` of 400 000 for the compiled-caller half and the mirror
figure for the interpreted one. A count beside the number, in the tests as well
as in the measurements.
