# `ir_lower.rs` proposals — round 10, lane `irlow`

Concrete mechanisms, not restated goals. Each entry names the file/function it
touches, the cost, and what would have to be measured before it lands.

## 1. A compile-time assertion that every raw-`CALL` route services its callee

**What.** This round found that `emit_self_recursive_call` was the one raw-CALL
route (of four: `emit_direct_cross_call`, `emit_cycle_edge_cell_call`,
`emit_inline_cache_call`, `emit_self_recursive_call`) that never called
`emit_inline_callee_deopt_service` before propagating the `i64::MIN` sentinel —
silently skipping the recursive callee's own exception-table check. The bug was
findable only by reading all four call-emission functions side by side and
noticing the asymmetry; nothing forced them to agree.

**Mechanism.** Add a `#[cfg(test)]` structural test (already the pattern this
file uses for `ops_that_define_a_result_slot` / `op_defines_result_slot_matches_the_arms_that_allocate`
at `jit/src/ir_lower.rs:38510`-38916): grep the function body source of every
`fn emit_*_call` (found via a `const` list of their names, or a proc-macro-free
string search over `include_str!(file!())`) for the literal
`push_call_exc_patch` call, and assert that any function containing it *also*
contains either `emit_inline_callee_deopt_service` or an explicit comment tag
opting out (e.g. `// NO-DEOPT-SERVICE: <reason>`). This is the same style
`ops_regalloc_calls_value_defining` already uses to keep two independently
maintained match arms in the same file from drifting apart — extended from
"two arms of one match" to "N sibling call-emission functions."

**Cost.** One test, no runtime cost. Maintenance cost: a new raw-call route that
*legitimately* doesn't need the service (e.g. a future leaf-helper call that
can never itself be a Java invoke) has to add the opt-out comment, which is a
one-line, self-documenting tax.

## 2. Fold the four call-emission functions' common "service, then re-check sentinel" tail into one helper

**What.** `emit_direct_cross_call`'s merged branch, `emit_cycle_edge_cell_call`,
and (after this round's fix) `emit_self_recursive_call` now all do the same
three-step dance after their `CALL`: call `emit_inline_callee_deopt_service`,
then re-run `emit_cmp_rax_sentinel` + `JNO` to see whether the service resolved
the sentinel, then fall into the ordinary keep/bail logic. Right now that
middle step is hand-inlined at each of the three call sites (three copies of
"cmp; jno; patch; ...; patch again"), which is exactly the shape that let the
fourth site (`emit_self_recursive_call`) drift out of sync in the first place.

**Mechanism.** A new private helper,
`fn emit_serviced_call_tail(&mut self, info_ptr: usize, num_args: usize, inputs: &[NodeId], keep_patches: &mut Vec<usize>)`
that emits exactly the "service, then fresh sentinel recheck, push a keep-patch
into `keep_patches`" sequence once, and have each of the three call sites push
their resulting patch offset into a `Vec` the caller resolves against one
shared `.keep:` label the way `emit_self_recursive_call` already resolves two
patches (`keep_patch`, `serviced_patch`) against one `keep_off`. This is a
refactor, not a behavior change, so it is exactly the kind of edit this round's
hard rules forbid doing blind (touches three call sites at once, no build
available) — proposed for a follow-up round with a build in hand, not attempted
here.

**Cost.** Neutral on code size (removes ~10 duplicated lines from two sites,
adds one ~15-line helper); removes the exact class of drift this round found.

## 3. A `CRATONVM_DBG_IR_CALL_SERVICE` census counter

**What.** `jit_service_callee_deopt` (`vm/src/jit/helpers.rs:4195`) either
resolves the sentinel into a genuine value (the callee caught its own
exception/deopt) or leaves it propagating. Right now there is no visibility
into how often each raw-call route's service call actually *does* something —
which would have made this round's gap visible from telemetry rather than only
from reading, the same way `IR_POLLS_OUTLINED` / `ir_osr_entry_skipped_census`
already give visibility into other rare-but-important code paths in this file.

**Mechanism.** Two `AtomicU64` counters next to `IR_POLLS_INLINE`
(`jit/src/ir_lower.rs`, file-level statics near the safepoint-poll counters):
`IR_CALL_SERVICE_RESOLVED` and `IR_CALL_SERVICE_PROPAGATED`, incremented from
Rust after the two outcomes are distinguishable — which is on the *VM* side
(`jit_service_callee_deopt`'s `Some`/`None` arms in `vm/src/jit/helpers.rs`),
not emittable as machine code from `ir_lower.rs` directly. So this is actually
a `vm/src/jit/helpers.rs` change (out of this lane's owned file) with an
`ir_lower.rs`-side companion: a debug flag
(`CRATONVM_DBG_IR_CALL_SERVICE`, following the existing `CRATONVM_DBG_IR_*`
convention) that, when on, has each of the four call-emission functions stamp
which route (self-recursive / direct / cycle-cell / inline-cache) is about to
call the service, so the two counters can be broken down by route without a
second call-site parameter threaded everywhere.

**Cost.** Two atomics, relaxed-ordering increments on an already-cold path
(the service is only ever called once RAX is confirmed to be the sentinel), so
no hot-path cost. Answers, on a real corpus, whether self-recursive methods
that throw/deopt internally are common enough that this round's fix is more
than a correctness nicety — i.e., whether it also matters for the interpreter
fallback rate reported elsewhere in this file's `unreachable_points_skipped`
family of counters.

## 4. A `probes/` test that actually exercises a self-recursive method with an internal catch

**What.** The existing self-recursive-call test
(`self_recursive_call_balances_its_shadow_push_and_reload`,
`jit/src/ir_lower.rs:30425`) only checks that the shadow push/reload balance;
nothing in the suite compiles and *runs* a self-recursive method whose body
catches an exception thrown by the recursive call itself, which is exactly the
shape this round's fix changes the behavior of. `jit/src/ir_lower.rs`'s own
test harness (`lower_inner` plus a hand-built `Graph`) cannot express a `try`/
`catch` around a recursive invoke — there is no `Op::Catch` in this IR at all
(catch semantics are implemented by deopt-to-interpreter, resolved by the
*interpreter's* own exception-table walk after the JIT frame unwinds), so this
is not a unit test this file can host.

**Mechanism.** A `probes/SelfRecCatch.java`-style end-to-end probe: a static
method that recurses N levels deep, has a `try { return f(n - 1) + workThatCanThrow(); } catch (ArithmeticException e) { return fallbackFor(n); }`
shape, and asserts (from Java) that the exception is caught at the *correct*
recursion depth rather than unwinding past every frame to the top — run once
with `CRATONVM_JIT_IR_SELFREC_DIRECT=1` (the direct route this round's fix
touches) and once with it off (the `jit_invoke_dispatch` route, which was
already correct), diffing the two for the fix's absence/presence. This belongs
in the VM's probe suite (`probes/`), not in `jit/src/ir_lower.rs`'s own
`#[cfg(test)]` block, and needs a build — proposed for whoever picks up this
round's build/test pass.
