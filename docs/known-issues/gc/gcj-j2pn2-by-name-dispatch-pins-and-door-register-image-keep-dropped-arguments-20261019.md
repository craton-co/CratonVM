# The by-name dispatch's argument pins and a door's saved register keep a dropped argument alive (default path, Generational jdk-only)

> **STATUS (2026-10-20, gcj j2/pn2, j3a rows): BOTH HOLDERS CONFIRMED BY THEIR SWITCHES; the page's two holders close with the two flips, but the probe keeps one more failing shape that is not this page's (see h4jm).** `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS=1`: private-special 0/3 failing (def 3/3, pr 3/3), super-special 0/6 with it (1/3 without), positive control fired (`[dispatch-tally] by-name-tail-unpinned` rows in `pnj2_argsp_both_cen.err`); soundness `pnj2_h4s_jor_can`, `_gau`, `_cpt` SAME (the G1 / ZGC H4S rows were not in the j3a list: rerun them before the flip). `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE=1`: static-direct 0/6 with it (def 2/3, p1 1/3) -- the door-image holder moved from spliced (j1a) to static on j3a, attribution by switch pattern only. With both: `FAIL 1 of 6` spliced-direct 4/4, held by `relay`'s own frame (compiled: its in-map parameter home; interpreted: the i10/np type-map root), not by a pin or a door image. Verdict for the orchestrator: P1 is ready to flip on its H4S rows plus the G1 / ZGC ones; PROMISE flips only with its own soundness list.**

> **STATUS (2026-10-19, gcj j2/pn2): OPEN -- holders named from the j1a census; both are freed by EXISTING opt-in switches whose default flip is the orchestrator's; rows in `docs/internal/gc-round-gcj-20261019/j2-pn2-report.md` block H4.** Retention only (an argument the callee nulled survives `System.gc()`); HotSpot frees it. Successor of `docs/internal/gc/gch-h4jm-gen-argsp-virtual-and-interface-direct-reopen-with-both-weak-doors-RETIRED-UNFIXED-20261018.md`.

## Evidence

`C:/craton/gcj-runs/j1a/vj1-pn/pnj1_argsp_cen.err` (`Gcd1ArgPinSpecialProbe more settle`, IR calls off,
`CRATONVM_DBG=gc-stats,oldmark-root-census,root-source`, `-XX:+UseGenerationalGC -Xmx64m`): stdout
`FAIL 2 of 6` (private-special, spliced-direct), HotSpot `PASS all 6`.

1. **private-special** (majors #2-#4): `holder#1 ... cat=4 ... /native-pin` on the judged `[J`,
   `door pinned-markers: 3/5`, compiled frames `Derived.privateCaller@sp-osr:5 ->
   Derived.dropPrivate@ir:26`. The call is a compiled kind-1 site served by name:
   `vm/src/jit/helpers.rs` `jit_invoke_dispatch_body` arm `1` -> `vm/src/vm/vm_exec.rs`
   `invoke_special_shared_on_class` -> `invoke_on_class_shared_no_retarget` ->
   `invoke_on_class_shared_inner`, whose `PinnedDispatchArgs` (~58746) pins the receiver and the
   array for the whole call. The tail's `interpreter::execute` pins its own copy, which the
   default-ON first-call weak door (`jit_bridge.rs` `WeakExecuteArgs`) releases; the dispatch
   window under it stays. `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS` (`release_before_rooting_tail`,
   vm_exec ~64624) releases it; it is opt-in since gch h1 "because alone it frees nothing",
   measured while the first-call door was still strong.
2. **spliced-direct** (majors #9-#11): `interp-local-live prov="method=...dropStatic:([JZ)Z off=488
   region=callee-saved-gpr-image reg=r13 caller=native claim=Keep tier=sp in_map=false"` -- the
   compiled callee's save of the Rust door's r13 (interpreted `relay` -> compiled `dropStatic`
   through `execute_jit_call`). `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE` (`door_promise_claim`)
   drops exactly such a word when it equals a weakly handed argument; its deciding list
   `vi14-od.list` was never run.

## What would close it

Flip `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS` (collector-agnostic: run its G1 / ZGC rows) and, after
its own soundness list, `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`, when block H4 / H4S pass. The other
three `PinnedDispatchArgs` sites (`vm_exec.rs` ~45332, ~45429 `invoke_or_native_keeping_args_current`,
~57272 `jni_jit_vsite_memo_call`) are not on this probe's path; the last two already skip their pins
under the switch.
