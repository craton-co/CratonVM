# j27 proposal: a callee whose body holds an argument-taking string concat is never spliced

> **j27 wave 8 (lane W8SPL, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION (no code change; still opt-in).** Read `vm/src/runtime/interpreter/jit_bridge/splice_concat_rows.rs` and `jit/src/splice_concat_bridge.rs`. The resolver admits only a `JIT_INDY_SITE_CONCAT` site with >= 1 argument and a reference result, interned under the callee's own (method, bci) -- the site its own compiled body calls -- and handler-row / synchronized bodies keep refusing. One planner gap by reading, perf only: `planner_concat_step` walks the body in pc ORDER, so in a loop-bearing body a store at a higher pc that executes before the concat on the next iteration (`for (..) { s = "x" + i; a[i] = s; }`) is not seen as "a store before the concat"; the builder's committed-store fence then refuses the whole METHOD build (the outcome the planner check exists to avoid), not a wrong answer. Host check on the `=1` arm: `CRATONVM_DBG_JITC=1` lines naming a committed-store refusal in a method whose splice holds a concat row inside a loop; if seen, make `planner_concat_step` treat any store in a body with a back edge as preceding every concat. Verify recipe unchanged (wave-4 block).

> **j27 wave 4 (lane SPLICECAT, 2026-10-28): FIXED IN CODE behind the opt-in `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1`, PENDING HOST VERIFICATION.** Opt-in because the change moves where operand `toString()`s and their exceptions run (inside the caller's compiled frame), which only a host run of the probe can show matches HotSpot. **What changed:** (1) VM resolver: `resolve_inline_site_from` asks `jit_bridge/splice_concat_rows.rs` `splice_concat_row` before the capture/refusal branch: a non-`()L` indy whose interned bridge site (`interned_jit_indy_bridge_site`, keyed by the callee's own method and bci, the site the callee's own compiled body calls) has kind `JIT_INDY_SITE_CONCAT` and whose call type returns a reference with at least one argument becomes the row `(pc, count_param_slots(desc), b'L', site)`; both scan arms (optimizing and single-pass) reach it; the existing `!sync_splice` scan gate keeps synchronized bodies out. (2) Rebase (`jit/src/lib.rs` `append_ir_inline_site`): the row check accepts `arg_entries > 0` only through `splice_concat_bridge::indy_row_shape_admitted` (switch on, `ret_tag == b'L'`); handler rows / synchronized still refuse (`ir-splice-indy-with-handlers` / `-synchronized`). (3) Planner: `ir_splice_body_may_trap_after_a_call` counts a concat row as a call (a later trap would replay the invoke, and with it the `toString()`s) and refuses the site when a store (`putfield`/`putstatic`/array store) precedes the concat in pc order (the builder's committed-store fence would otherwise refuse the whole METHOD); `ir_splice_tree_leaves_a_call` answers `true` for a body with a concat row; the constant-word exemption now requires `args == 0`. (4) IR builder: `indy_bridge_site_at` admits `spliced_concat_indy_at` rows; `push_indy_bridge_call` builds the same `Op::IndyBridge` the method-own site builds (same site, operands popped deepest-first, `[ctrl, mem, arg0..]`) and calls `note_splice_call` for a spliced concat: the replay fence is raised and the successor gets the post-call chain snapshot (the callee frame at `pc + 5` with the `String` on its operand stack) -- the deopt state for a post-call exit. The lowering is unchanged: safepoint map before staging (the bridge pins every staged word before it can allocate; live references of the caller and callee are in the map like at any call), inline-frame row at the return address (so a `toString()` exception's trace names the spliced callee at the concat's line), the `i64::MIN` exception edge resolved at the enclosing invoke (a `toString()` exception or an OOME of the concat propagates as from a spliced `Op::Call`; no replay, since an exception edge is a rethrow, not a deopt). (5) Single-pass splice (`jit/src/x64/inlining.rs` `0xba` arm): `emit_spliced_concat_bridge` stages the operands like the spliced dispatch call (one shared `emit_inline_buffered_helper_call`, extracted from `emit_inline_dispatch_call`: buffer at the pre-pop cursor, reference operands named in the map, inline-frame row at the return address), calls `DirectHelperTable::indy_bridge`, checks the sentinel, pushes the `String`, then files `emit_splice_post_call_exit_site` as after any spliced call. **Probe:** `tools/probes/jit/j27/J27SpliceConcatRows.java`. **Verify:** `J27SpliceConcatRows` stdout must diff clean against HotSpot in `--jdk-only` and `--compatible`, with and without `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1`, and under `--nojit`; `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1 CRATONVM_DBG_JITC=1 cratonvm --cp <dir> J27SpliceConcatRows 2>&1 | grep 'splice-concat-row ADMITTED'` must show rows for `J27SpliceConcatRows.tag`, `.label`, `.show`, `.mix` (and, where the planner admits them, `.afterConcat` / `.throwAfter`), and `grep 'capturing or non-reference site: no constant word'` must no longer name them; `J27IndyTrapRows` stays unchanged against HotSpot. Time `-Dj27.timing=true` (`time splicecat-hot`) with the switch off and on. Unit tests: `jit` crate `splice_concat_bridge::tests::{splicecat_a_concat_row_is_rebased_only_under_the_switch, splicecat_the_planner_counts_a_concat_row_as_a_call, splicecat_row_shapes, splicecat_a_spliced_concat_builds_as_its_bridge_with_its_operands}`, `x64::tests::j27_splicecat_a_single_pass_splice_calls_the_concat_bridge`; `vm` crate `runtime::interpreter::jit_bridge::splice_concat_rows::tests::splicecat_concat_row_shapes_count_one_entry_per_value`. Note: in the OPTIMIZING tier a straight-line `if (bad) throw new X("..." + v)` arm is already masked (`mask_cold_throw_arms_keeping`), so `Base.pos` of the parent page had no indy left to refuse there; this change matters for concats on a hot path, on a non-throwing cold arm, and for every concat in a SINGLE-PASS splice (no masking there).
>
> Expected HotSpot 25 output of `J27SpliceConcatRows` (derived by hand from the JLS and `Float`/`Double.toString`; frame lines are the probe file's own; confirm with the reference run):
>
> ```text
> fact splicecat-hot-sum 5154816
> fact splicecat-cold rare=-7/999999
> fact splicecat-show o=g,v=7
> fact splicecat-null o=null,v=7
> fact splicecat-throwing-tostring java.lang.IllegalStateException bad toString
> fact splicecat-throwing-tostring-frames J27SpliceConcatRows$Bad.toString:35<J27SpliceConcatRows.show:64<J27SpliceConcatRows.showLoop:114
> fact splicecat-mix c=A,f=0.33333334,d=0.30000000000000004,u=Ω
> fact splicecat-mix-exp c=z,f=1.0E10,d=1.0E-5,u=é
> fact splicecat-mix-utf16 c=Ω,f=-0.0,d=NaN,u=q
> fact splicecat-deopt-sum 4999989
> fact splicecat-deopt-tostring-count 1000000
> fact splicecat-throw-after java.lang.IllegalStateException t=-9
> fact splicecat-throw-after-frames J27SpliceConcatRows.throwAfter:84<J27SpliceConcatRows.throwLoop:138
> ```

| | |
|---|---|
| **Status** | FIXED IN CODE behind the opt-in `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1` (j27 wave 4, lane SPLICECAT), PENDING HOST VERIFICATION. Proposal filed 2026-10-28 by j27 wave 4 lane INDYTRAP. |
| **Area** | JIT splice resolver `resolve_inline_site_from` (`vm/src/runtime/interpreter/jit_bridge.rs`, the nio21 wave 11 W10L3-1 indy block) and the IR splice table check (`jit/src/lib.rs`, `site.ir_indy_rows`, `ir-splice-indy-row-mismatch`). |
| **Parent** | [j27-osr-splice-bind-refuses-cold-indy-trap-callee-proposal-20261028](j27-osr-splice-bind-refuses-cold-indy-trap-callee-proposal-20261028.md) |
| **Probe** | `tools/probes/jit/j27/J27IndyTrapRows.java` rows `indytrap-splice-*` (`Mid.pos` over `Base.pos`), `J27OsrSuperRows` `osr-user-throw` |

## Evidence (by reading)

The parent page blamed the reason-8 indy trap for `Base.pos(int)` (`if (v < 0) throw new
IllegalArgumentException("neg: " + v)`) never being spliced or direct-bound. A javac concat has no
such trap: its bridge site is built from the pool at compile time
(`make_jit_string_concat_site_from_parts`), so `has_indy_trap` is false. What does refuse the
SPLICE is the resolver's indy admission:

* `resolve_inline_site_from` admits a spliced `invokedynamic` only as a `()L` constant-word lambda
  site (or, optimizing tier with `CRATONVM_JIT_IR_SPLICE_INDY_CAPTURE`, a linked capturing lambda
  whose proxy the VM names). Any other descriptor reaches
  `no!("invokedynamic (capturing or non-reference site: no constant word)")` -- an `(I)String`
  concat included, cold arm or not.
* The IR table check then requires `arg_entries == 0` and `ret_tag == b'L'` for every row
  (`ir-splice-indy-row-mismatch`), and the builder bails a spliced indy it cannot bridge
  (`if !self.splice.is_empty() { return ir_build_bail(..) }`).

So every small user method that builds a message with `+` on any arm is unspliceable.

## What would fix it

Admit a `StringConcatFactory` bridge site (`jit_indy_site_kind == JIT_INDY_SITE_CONCAT`, non-zero
from `interned_jit_indy_bridge_site`) of any arity as an `ir_indy_rows` row `(pc, arg slots,
b'L', site)`, behind a new opt-in switch (say `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`), and widen the
table check to accept `arg_entries > 0` for a concat-kind site only. The builder's
`push_indy_bridge_call` already takes `arg_entries`; the concat bridge is a fallible helper call
(a `toString()` may throw), so a body with handler rows or a synchronized one must keep refusing
exactly as the lambda rows do (`ir-splice-indy-with-handlers` / `-synchronized`). Check the
spliced call's deopt state: the bridge's exception edge must unwind through the splice's frame
chain like any other spliced call. Owners: the splice-resolver lane (not INDYTRAP: the resolver is
outside the concat lowering). Verify with `J27IndyTrapRows` (output unchanged against HotSpot) and
`CRATONVM_DBG_JITC=1` no longer printing the refusal for `Base.pos`; time
`-Dj27.timing=true` before and after.
