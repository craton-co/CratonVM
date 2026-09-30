# Proposal: a compile request returns why it produced no body

**Status: proposal — filed 2026-10-03 by interpreter round i1 wave 39, lane
L2. Not implemented.**

## The problem

Every compile door asks the jit crate for a body and gets back an `Option`.
When it is `None`, each door has to guess why, and the guesses disagree:

* `try_compile_request` knows whether the backend ran (`backend_attempted`)
  and bail-lists only then, but returns the bare `Option`; the reason is
  left in thread-locals (`take_jit_bail_site`, `take_jit_pipeline_stage`,
  `last_compile_fell_through_to_single_pass`, `ir_evidence::take_last_verdict`)
  that a caller must drain in the right order, on the right thread, or the
  next compile reads them.
* The optimizing OSR route (`build_osr_optimizing_artifact`) memoises every
  `None` method-wide until the next redefinition, a resolver miss (a callee
  class not loaded yet) exactly like a backend refusal, because it cannot tell
  them apart (`i38-L2-compile-door-review-items-left-open`, item 2).
* The eager first-call door (`execute`) seals every `None` it does not
  recognise as transient into the name-keyed `jit_skip_set`; wave 39 taught it
  one transient case by hand (a static-field resolver miss, with a per-method
  retry bound), and the next one will be taught by hand too.
* The background worker classifies its own `None` from the bail list and the
  OSR-denial set after the fact (`background_compile_task`'s `permanent`).

## The proposal

Return a structured outcome from the one compile funnel:

```rust
pub enum CompileOutcome {
    Compiled(CompiledMethod),
    /// A constant-pool operand or callee the resolvers could not name
    /// without loading: retry after the method has run interpreted.
    ResolverMiss { site: BailSite },
    /// A cap or a verdict that expires (code cache, run-time de-speculation).
    Transient { why: &'static str },
    /// The backend ran and refused: a property of the bytecode.
    Refused { site: BailSite },
}
```

and let each door map the four cases onto its own memo with one shared rule:
`Refused` is the only permanent verdict; `ResolverMiss` is retried a bounded
number of times per method (the worker's `tier_fail_count` and the eager
door's `EAGER_RESOLVER_MISS_RETRIES` are two copies of that bound today);
`Transient` is never memoised. The thread-local verdict slots then become
fields of the outcome and stop being a cross-compile hazard.

## What it would close

* Item 2 of `i38-L2-compile-door-review-items-left-open` (the route's
  resolver misses), without a new thread-local (the jit crate's statics
  ratchet is exact).
* The hand-taught transient cases of the eager door, and its remaining
  resolver `?`s, in one place.
* A census that can say, per door, how many methods were refused for good
  and how many are waiting on a class to load — the number every "why is
  this method interpreted" investigation starts by reconstructing from
  `CRATONVM_DBG_JITC` lines.

## Cost and risk

Compile time only; no per-call path changes. The work is mechanical but
wide: every caller of `try_compile_request` / `compile_optimizing_artifact`
/ `jit::try_compile` and the tests that pin their `None` arms. Do it
through `i37-L2-proposal-one-invoke-site-classifier-for-every-compile-door`'s
door table if that lands first, so the four doors share one mapping.
