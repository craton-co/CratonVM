# The j27 flip of `_SPLICE_ANEWARRAY_LOOP_CALLEES` makes wave 13's loop-callee rule inert by default

Status: OPEN (needs a host A/B). Area: JIT splice resolver (`jit/src/ir.rs`
`splice_anewarray_refuses_loop_callee`, `vm/src/runtime/interpreter/jit_bridge.rs` `resolve_inline_site_from`).
Severity: possible perf regression on `cached` / `matchonly`; no correctness issue. Found by rx27 lane R,
2026-10-28, at `df86fe90ba`.

## Evidence

* `CRATONVM_JIT_IR_SPLICE_ANEWARRAY` and `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES` are both read with
  `runtime_flag_default_on` (`jit/src/ir.rs` `ir_splice_anewarray_enabled`,
  `ir_splice_anewarray_loop_callees_enabled`), marker `WARNING j27-user-flip-20261028`.
* `_LOOP_CALLEES` exists only to restore wave 10's admission for an A/B. ON, `splice_anewarray_refuses_loop_callee`
  always answers `None`, so W13-6's fix (a loop-rooted `anewarray` callee such as
  `Pattern.split(CharSequence,int,boolean)` keeps its own compile) never runs by default.
* The last measurement of exactly that state is on `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`,
  "Round 25 wave 13": `cached` +12.6 %, `matchonly` +8.3 %, `split-ws` +2.9 % against the default of the time.
  No later measurement with both flips is on either page.

## What would close it

One host A/B (`icount2.sh`):

```
icount2.sh rx27-anew 'regex-matches cached matchonly split-ws regex-split' \
  hs:hs: def:TAG: w13:TAG:CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=0 \
  off:TAG:CRATONVM_JIT_IR_SPLICE_ANEWARRAY=0
```

If `w13` wins on `cached` (as c26/c31 did), set `_LOOP_CALLEES` back to opt-in (`runtime_flag_on`, its
`flag_groups.rs` row stays) and keep `_SPLICE_ANEWARRAY` ON. Engagement under `w13`:
`CRATONVM_DBG_JITC=1 ... cached | grep ir-splice-anewarray-loop-callee` names
`Pattern.split(Ljava/lang/CharSequence;IZ)`.
