# AotIntegrationTests: one intermittent SIGSEGV on the wave 5 binary (2026-11-07)

`org.springframework.test.context.aot.AotIntegrationTests` (spring-test, `--jdk-only`, Linux host 64.177.43.150) died once with SIGSEGV after 16 s on the wave 5 binary (`281d9c617e`), in a run of three-plus on that binary; every other run of the class passed (104 s, 2 passed, 2 skipped). The wave 4 binary passed it once.

Crash report: `test byte [rax+6],3` in a live registered code buffer, `rax=0x0c00000000000275` (not a reference), `si_code=0x80` trapno 0xd, `unpinned_jit_entries=0`, no recently freed code buffer at the pc. A corrupt reference in compiled code, after four `BeanOverrideBeanFactoryPostProcessor` log lines. No `hs_err` detail was written (193-byte file).

Suspect, not proven: W5A's door-form push for nested compiled entries (`JitEntryGuard::nested_door_forms`), which skips some per-entry bookkeeping. The switches did not make a difference in two bisect rounds (the crash did not recur, so this proves nothing). The nested door form had no measurable effect on the p4 harness (13.1/9.2 s vs 13.5/9.3 s with the three W5 switches off), so `enter_with_compiled` is back on the long form; `nested_door_forms` stays, unused.

To do: loop the class (or a smaller GC-heavy reproducer) 20+ times on both the held-back and the door form; if it still crashes with the long form, the cause is W3-W5 elsewhere (W5B negative memo, W4A coerce plans, W5C tiny ctor warmup).

## Update: it is a dev bug in C2 `Pattern$Start.match` (not a wave regression)

Looped `AotIntegrationTests` (40 s cutoff) on the dev base `332b13ac02` and the wave 2, 3 and 4 binaries: 9, 11, 7 and 8 crashes in 60 runs while the host was loaded; the wave 5 binary crashed too. Every crash has the same registers (`rax=0x0c00000000000275`, `rdi=0x20057c0f080`) and the same instruction, so the door-form revert did not matter and no wave caused it.

What the 150+ captured crashes show (gdb `catch signal SIGSEGV if si_code==0x80`, `CRATONVM_JIT_PERF_MAP=1`):
- All 23 mapped crashes are in the C2 body of `java/util/regex/Pattern$Start.match(Matcher,int,CharSequence)Z`.
- The faulting instruction is the first instruction of an inlined `instanceof` (`testb $3,6(%rax)`, then `cmpl $0x...112,0(%rax)`) on a value read from `0x10(O)`, where `O` is the result of the generic `jit_getfield` helper (`this.next`). That read and the type test sit before the `ExactClassIs(O, 0x7fd)` test of `O` is consumed, so when `O` is a different Node subclass the word at `O+0x10` is not a reference (here another object's header word) and the test faults.
- Not involved: GC (large young generation still crashes; GC switches off still crash), the door-form push, the eager-chain sub-switches (declaring key, pending stamp, uninit-new trap, chain align, has-run, thin profile, retire-asks-forward, one-owed-retry), `inline-getfield`, `field-site-cache`, `cha`.
- Involved: `CRATONVM_JIT=-eager-callee-chain` gave 0 crashes in 60 runs, and `CRATONVM_BG_COMPILE=0` gave 0 in 56 (runs long enough to pass the crash point). The read looks like a planted pre-header read (`ir_optimize/seq_charat_predicate.rs` `plant_identity_tests` / `plant_field_class_predicates`, `loop_field_forward.rs`, `plant_null_base_load_predicates` in `ir_optimize.rs`) that is ordered after the class guard only by node id, not by a licensing-guard input (`with_licensing_guard`, `ir.rs`). Also noted: `anchored_on_every_iteration` (`ir_optimize.rs`) accepts a Proj of any `If` on the header without checking the other edge leaves the loop.
- A simple 10-pattern regex probe (`apps/javac-aot-fixtures/p5/RegexStartMatch.java`) did not reproduce it (30 of 30 clean).

The host stopped reproducing it late in the session (controls 0 of 60 and 0 of 24 even with 16 CPU burners), so the arms `-inline-calls`, `-inline-nest`, `-inline-splice-devirt`, `-field-fast-path` (0 of 50 and 0 of 60 vs controls 0-1) are unproven. Next: loop the `-ir-seq-charat-*` and `-ir-null-base-load-predicate*` switches as arms when the rate returns; check with `CRATONVM_DBG=ir-licm` whether the `O+0x10` load carries the licensing guard as its fifth input.

## Update 2026-11-07 (later): second defect fixed, root cause still unconfirmed

`anchored_on_every_iteration` now takes the loop body and refuses a projection of the header's `If`
when the sibling projection stays in the loop (an in-body branch arm may be skipped). Test:
`anchored_on_every_iteration_refuses_an_in_body_branch_arm`. This is a soundness fix found by code
reading; it is NOT shown to be the SIGSEGV cause (no reproduction after the host stopped crashing).
Still open: whether the `O+0x10` load in `Pattern$Start.match` carries the licensing guard
(`CRATONVM_DBG=ir-licm`). Separate finding: 19 `ir_optimize` lib tests are red on dev itself (stale
`*_is_opt_in` pins after the default flip, e.g. `strip_mine::tests::j27_*`, `r19_osr*`), same set with
and without this change.
