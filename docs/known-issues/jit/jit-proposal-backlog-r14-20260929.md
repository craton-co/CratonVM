# JIT proposal backlog after the round-14 triage (r13 books + the r13 backlog)

Status: OPEN (the single ranked backlog of JIT proposals still worth building; ideas, not work items until the owner queues them)
Area: the JIT and its VM side (calls, splices, deopt/replay, monitors, `--jdk-only` dispatch, `java.math`, FFM)
Severity: index
Found by: round 14 wave 1 lane ptriage, 2026-09-29

Source: every entry of the 77 round-13 books `jit-r13-*-proposals.md` and the top-25 / runners-up /
a 20-entry sample of `jit-proposal-backlog-r13-RETIRED-20260929.md`, triaged against the code in
`docs/internal/jit-proposals/jit-proposal-triage-evidence-r14-20260929.md` (496 entries: DONE 120,
KEEP 210, DUPLICATE 78, SUPERSEDED 11, REJECT 77). This page carries the KEEPs of value **med or
higher** plus every **correctness** item (any value). Low-value KEEPs stay in the evidence file as
the record. The r13 backlog's tier-B/C entries that were not sampled keep their r13 verdicts; the
sample (20) found 7 of them DONE or superseded, so re-check an r13 tier-B/C entry in the code
before scheduling it.

Default-OFF switch decisions are not here: `docs/internal/jit-proposals/jit-optin-switch-triage-r14-20260929.md`
(two flips are ready to run: `CRATONVM_JIT_IR_PRUNE_LOOP_HEADER_LOCALS` and
`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER`).

Round 14 wave 1 is in flight in the same worktree: CC5-1, M2-1, W11-2, the upcall direct-static
lane and the compat7 findings landed there (pending build). The wave-1 lanes' own new books
(`jit-r14-calls`, `jit-r14-ffm`, `jit-r14-sync`) are not ranked here; three of their entries restate
r13 entries and are noted in the table.

## Top 15 (ranked by value against cost; **W2** = recommended for this round's wave 2)

| # | entry (book) | value / cost | why now | files it would touch |
|---|---|---|---|---|
| 1 **W2** | SR-1 splice a trap-free `synchronized` callee between `MonitorEnter`/`MonitorExit` (r13-syncres; = S13-2, r13-backlog #19 W9-1) | high / med | `SyncM` is ~110 ms a round against HotSpot's 32 with every monitor sequence already inline: what is left is the two CALLs per iteration. The resolver still refuses every synchronized callee (`jit_bridge.rs:20146` `no!("synchronized")`); the caller-held route's closed-body predicate already proves the trap-free half. Handoff §4 item 2. | `vm/src/runtime/interpreter/jit_bridge.rs` (`resolve_inline_site_from`, minimal: interpreter round shares the file), `jit/src/ir.rs` (splice builder, enter/exit around `push_splice_state`), `jit/src/ir_lower.rs` (refuse a deopt between enter and exit) |
| 2 **W2** | SH5-1 a retired row stops the superclass walk, strict mode only (r13-shadow5) | high (correctness) / med | Closes the 19 half-retirements of `r13w13-shadow5-ancestor-bridges-over-retired-rows` by construction and unblocks the `AbstractCollection` / `OutputStream` / `Buffer` retirements (handoff §4 item 5). `--compatible` is untouched (the set is empty there). | `native-api/src/registry.rs` (refused-as-retired set + `was_retired`), `vm/src/vm/vm_exec.rs` `invoke_or_native` walk (minimal), `vm/src/runtime/interpreter/dispatch_virtual.rs` populate walk; kill switch `CRATONVM_JDK_ONLY_RETIRED_STOPS_WALK` |
| 3 **W2** | GS-1 bimorphic sites: two guards, then the call (r13-guardsplice) | high / med | The W7-1 profile path plans nothing below an 80 % class (`jit/src/ir.rs:17070-17090`); `List.get` over two lists and `Map.get` over `HashMap`/`LinkedHashMap` are the common shape. First step is the census the book names. | `jit/src/ir.rs` (guarded multi-return frame, "open the pending frame on close"), `jit/src/lib.rs` (planner row with two bodies) |
| 4 **W2** | R13RP6-1 the x64 framed trap names its artifact through its epoch guard (r13-replay6; = R13-8, R4-1, R13R5-1) | med (correctness) / small | The only re-run from entry the census still allows by design at the call-site services (a hot-swapped callee's superseded body). No ABI change on x64: `DeoptEpochGuard` (`jit/src/deopt.rs:1334`) is already the stub's 4th argument. | `jit/src/deopt.rs` (guard field + `StashEntry` field), `jit/src/x64/deopt_stubs.rs` (`x64_deopt_entry`), `vm/src/jit/helpers.rs` `try_resume_trapped_callee` (minimal) |
| 5 **W2** | CH5-4 + CH5-5 the chain arm toward neutral: `MULTI_RETURN` reader, then the guarded splice's miss edge as a trap (r13-chain5; GS-6) | med / med | Handoff §4 item 1: the chain arm is the largest lever left on `hashmap` (1957 ms vs 635) and is 7 % slower than the default. CH5-4 is a flip prerequisite (`ir.rs:16883` process `OnceLock`); CH5-5 makes a field-held map's receiver exact past the join, so `put`/`get` pay one class test instead of two. Pair with the chain-arm rebench (switch triage). | `jit/src/ir.rs` (reader; guarded splice miss edge -> uncommon trap, dominance-scoped fact), `jit/src/lib.rs` (planner evidence rule); same builder region as #3, so one lane |
| 6 **W2** | M8-2 bound `JitMICSlot::update`'s spin + M8-1 an IC-only quiescence stamp at the Rust miss handlers (r13-mega8) | med / small (M8-2), med / med (M8-1) | M8-2 is the one unbounded spin in the IC code (three lines, turns a future silent hang into a helper call). M8-1 fixes the open `r12w7-mega6-grace-starves-while-a-thread-stays-compiled` page without a stop-the-world. | `jit/src/lib.rs` (`JitMICSlot::update`, `JitThreadQuiescence`, `classify_thread_quiescence`), `vm/src/jit/helpers.rs` miss-handler prologues (minimal) |
| 7 **W2** | BD4-2 + BD4-1 `bigint_mul_pow10` through the 5^n memo; chunked then subquadratic `from_decimal` (r13-bigdec4) | med / small | BD4-2 is an exact two-line patch (`native-builtins/src/lib.rs:42045` still calls the uncached `bigint_pow5`); BD4-1's 9-digit chunk loop alone is a 9x win on every `new BigInteger/BigDecimal(String)` (`bigint.rs:134`: one full-magnitude multiply per digit). | `native-builtins/src/lib.rs`, `native-builtins/src/math_bignum.rs` (`pub(crate)` `bigint_pow10`), `native-builtins/src/bigint.rs` |
| 8 **W2** | CC3-1 deferred cold tails for the IC-hit and direct-cross-call sentinel paths (r13-callcost3) | med / med | Reuses CC2-1's landed machinery (`ir_lower.rs:636` `DeferredSelfCallTail`); every call-dense row (`hashmap`, BigDecimal kernels) pays a taken branch and ~120 cold bytes per site in the hot span. First step is the admission census. | `jit/src/ir_lower.rs` (a third tail variant, `emit_ic_hit_exit`, `emit_direct_cross_call`) |
| 9 | MISC11-1 + MISC11-2 move `ldc_global_slots` and `BOOTSTRAP_APPENDED_CLASSES` onto per-VM state (r13-misc11) | med / small | Two process statics carrying per-VM meaning (AGENTS.md: no process globals); each is -1 on the statics ratchets. | `vm/src/jit/helpers.rs:12715-12737` (interpreter-round shared file), `classloading/src/class_manager.rs:25971`, a `NativeContext` accessor in `native-api` |
| 10 | I7-4 fold `"literal".hashCode()` / `HashMap.hash("literal")` (r13-iropt7) | med / small | String-keyed lookups with constant keys are common in real code; only the box hashes fold today (`ea_ir_bridge.rs:2377`). | `jit/src/ea_ir_bridge.rs`, the `ldc` resolver (`JitLdcConstant::StringSlot` in `vm/src/jit/helpers.rs`) |
| 11 | OD-1 the OSR loop is hot evidence for the inline tier (r13-osrdoor) | med / small | An unprofiled `main` loop gets the cold inline budget for exactly the loop that triggered OSR (`lib.rs:20527` prices by `hot_loop_ranges` only). | `vm/src/runtime/interpreter/jit_bridge.rs` `compile_osr_body`, `jit/src/lib.rs` `plan_osr_door_splices` |
| 12 | CH3-4 charge an OSR chain guard exit at its outermost bci (r13-chain3) | med / small | `jit_bridge.rs:1586` returns uncharged for any chain, so a spliced guard that fails on every OSR entry is never de-spec'd; the chain arm makes this reachable. | `vm/src/runtime/interpreter/jit_bridge.rs` `charge_osr_guard_exit` |
| 13 | SH3-2 `java.util.Random` state in its real `seed` field (r13-shadow3; M10-5 follows) | med / med | Removes the `SEED_TABLE` process static (`securerandom.rs:220`) and a registry mutex per draw; fixes every native/bytecode state disagreement at once. | `native-builtins/src/securerandom.rs` |
| 14 | T3-3 per-class origin memo for `StackTraceElement` (r13-trace3) | med / small | Wave 13's module/loader fill asks the class manager per element (`lang_misc.rs:462` `frame_module`): every deep Spring trace paid for fidelity. | `native-builtins/src/lang_misc.rs` |
| 15 | FFM7-1 free `Arena.ofAuto()` blocks once the session is collected (r13-ffm7; = FFM5-1, r13-backlog FFM2-3) | med / med | The last unbounded leak of the default FFM model (`foreign_ffm.rs:3333` records nothing for a non-closeable arena); the wave-9 upcall-stub sweep is the template. | `native-builtins/src/phases_late/foreign_ffm.rs`, `native-builtins/src/panama.rs` |

Restated by wave-1 lanes (take the newer text when scheduling): CC4-1 = `jit-r14-calls` C14-2,
CC4-4 = C14-1, SR-3 = `jit-r14-sync` SY14-4.

## Correctness items (any value), not in the top 15

| entry (book) | value / cost | note |
|---|---|---|
| MF13-2 resolved-owner reflective dispatch (r13-mhffm; = r13-backlog #21 HW8-1) | med / small | `vm_exec.rs:31812` still selects by name walk; two same-named classes on one chain |
| R13RP6-3 one VM replay-verdict entry point (r13-replay6; EX4-1) | med / med | eight verdict copies; the round 12-13 replay defects were mostly a missed copy |
| EX4-2 property test that the builder's trap rule implies the sink's (r13-exc4) | med / small | a test, but for a rule broken in three rounds |
| CH-2 one stash-judging function for every sink (r13-chain2) | med / med | four sink copies; the chain arm is what makes them disagree |
| CC-1 an unowned baked entry unrepresentable (r13-codecache) | med / med | code-buffer lifetime family, next to `gcd-d10v` |
| CC-4 refuse a zero bake-time identity at publication (r13-codecache) | med / small | same family |
| SH3-1 "exact receiver only" registry attribute (r13-shadow3) | med / med | the `Random` species at the doors |
| SR-2 the exceptional channel of the self-lock hand-over (r13-syncres; S5-3) | med / large | high risk; `r14w1-sync-handler-sinks-conflate-the-method-monitor-with-local-0` must land first |
| r13-backlog #24 P7 arithmetic LICM across calls, find the Lucene bug | med / med | `x64/licm_int.rs:474-482` blanket refusal hides a latent miscompile |
| OD-6 direct binds inside an OSR splice through retire cells | low / med | one stale call after a redefinition |
| R13-9 lambda direct arm retires an impl after an unsound re-run | low / small | `jit_bridge.rs:23275` private to the doors |
| P5 judge `$ProxyN` arguments in `Method.invoke` / MH | low / med | runs the callee where HotSpot throws IAE |
| P6 retire the `ends_with("AnnotationProxy")` arms | low / small | `typecheck.rs:1886` fails open for `MyAnnotationProxy` |
| proxy4 P4 "no interface record" vs "zero interfaces" | low / small | `aastore` fails open for a zero-interface proxy |
| T2-3 singleton-OOM guard inside `attach_snapshotted_trap_frames` | low / small | `exceptions.rs:3437` |
| HC3-3 invalidate the String-node memo on node replacement | low / small | defensive; every JDK node replacement bumps `modCount` today |

## The rest of the med-or-higher KEEPs (unranked within a group)

**Calls and inline caches**: CC4-1 constant method handles (high / large); CC2-3 `CALL rel32`
for cross calls; CC3-3 roots in deferred tails; CC4-4 decoded MH record; CC4-5 stackless door
runs a lane-shaped handle (interpreter-owned file); CC5-3 static memo for by-name static routes;
CC5-5 shrink-wrap call-free `fib` activations (= R13C-9, LC-4, r13-backlog F8-1); R13C-2 intrinsic
ladder inside splices; R13C-1 `@IntrinsicCandidate` fallback census; GS-2 miss-edge IC seed; GS-3
larger guarded-body cap for hot sites; GS-4 guards for nested sites; M9-4 inline PIC for over-wide
single-pass sites; M9-5 seed spliced call sites from the callee profile; OD-2 guarded splices at the
OSR door; OD-5 retire cells admit a newer callee body (= r13-backlog #14 C12-1); M9-1 the
megamorphic counter run (script ready: `C:\craton\jitr14-probes\m91-mega-anatomy.sh`).

**Splices, chains, IR**: CH-3 stop re-splicing a trapping body (= CH5-6); CH4-1 caller-held hold
for a synchronized callee's resume; CH4-4 optimizing-tier self-locking bodies (= S13-13, S3-7);
FS-5 speculation inside spliced bodies; I7-2 unloaded type-check target does not refuse the splice;
I7-6 `Integer.valueOf(v).equals(k)` in the graph; SL-2 no header φ for a dead local (after the
header-prune flip); r13-backlog #7 explicit `athrow` path, #8 inline VarHandle field access, #10
IR direct-throw exits, #13 late-binding cells, #15 identity-keyed inlining deps, #16 static access
in LICM, #17 regalloc quality, #20 FFM element access in the IR tier, #23 one door for Rust calls
into compiled bodies, #25 `Math.fma` in the IR tier; W10-2 retire the unresumable-trap gate;
r11-calls 4 recursive instance self-call inlining; r11-fib F14-3 split-monitor rethrow frames;
r11-irstatic W10-1.

**Monitors**: M2-3 `notify()` wakes one waiter; M2-4 `Object.wait()` stops polling every 5 ms;
M3-2 the `SyncM` shape-by-shape measurement; S6-1 per-class mirror handle created with the mirror
(= S7-6); S6-2 weak `ldc` / mirror slots (class unloading).

**`java.math`, FFM, traces, collections**: BD-1 exactness harness per `java.math` Intrinsic;
FFM2-1 free shared arenas on close (= MF13-1); FFM3-5 per-handle downcall marshalling plan; T3-1 a
native frame for every throwing `ACC_NATIVE`; C5-2 no per-read copies behind a node entry view;
HT-3 / C7-1 `--compatible` families yielding to bytecode (**owner decision pending** on
`r13w13-compat7-chm-tree-bins-ht3-cost`; do not schedule); r11-g1store W17-2 G1 ref-store gates
(GC-owned).
