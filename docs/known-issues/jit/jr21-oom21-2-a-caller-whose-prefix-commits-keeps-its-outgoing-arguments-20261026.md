> **j27 wave 9 batch 4 (lane W9RPL, 2026-10-29): the holder is NAMED. The `store` / `long` rows are FIXED +
> VERIFIED (Windows) under the opt-in rule. `watched` / `volatile` / `param` remain the design gap. The page
> stays OPEN.**
>
> **Attribution** (Gen, `CRATONVM_DBG_OLDMARK_ROOT_CENSUS=1`, `cvm-rpl-3`). The batch-3 "interpreted frame"
> guess was wrong. The array is held by the caller's SINGLE-PASS frame at the call:
> - Rule on + `IR_INLINE=0`: `top#1 ... [B old_bytes=53526544 prov="method=J27OwnedArgsPrefixRows.storeCaller:(I)I
>   off=16 region=java-local tier=sp sp=10 in_map=true"`. Same for `longCaller`.
>   - The caller is single-pass because its IR body was lowered and then REFUSED by the acceptance gate
>     (`[ir] acceptance ... REFUSED (evidence: none) -- keeping the single-pass body`).
>   - So the IR `admitted=true owned=true` line seen in batch 1 was for a body that never ran.
> - Adding `CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS=1` drops the dead local `a`, and the holder becomes
>   `off=80 region=operand-spill tier=sp sp=10`. The single-pass compile logs `service-copies
>   J27OwnedArgsPrefixRows.storeCaller:(I)I bci=10 ret=1224 slots=[80]`. That is the call's service copy:
>   the single-pass owned hand-off refused the `putstatic` prefix because the constant-static rule was
>   wired only into the IR tier.
> - `R21OomArgWindow` `watched` (default arm): the holder is `watchedCaller` `off=16 java-local tier=sp
>   sp=17`. With SP map liveness it becomes `off=80 operand-spill`. Its prefix has an invoke
>   (`WeakReference.<init>`), so no re-run rule can hand it off. This is design 1 (weak copies), unchanged.
>
> **Fix** (opt-in, under the existing `CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES`; no new switch). In
> `jit/src/x64/op_invoke.rs`:
> - New `caller_replay_to_call_is_exact_with_statics`, and new `sp_static_row_is_plain` (a resolved,
>   non-volatile row of `Compiler::static_field_info`).
> - `Compiler::owned_args_handoff_replays_exactly` now asks the statics form. The old 4-argument form is
>   `#[cfg(test)]` for the gcd d9/d tests.
> - The service-copy window already counts these points (`owned_args_caller_replay_may_be_exact`, batch 1).
>
> **Tests** (`cratonvm-jit` lib, `x64::op_invoke::j27_w9rpl_sp_const_static_tests`, 2 tests): a resolved /
> volatile / unresolved / missing row; the rule admits `storeCaller` only under the switch and only with a
> plain row. Together with `gcd_d9d` and `j27_oom21`, 16 pass. They were written in the same edit as the
> code, not run red first.
>
> **A/B** (`cvm-rpl-4`, `-Xmx96m`; 5 interleaved runs x `--jdk-only` / `--compatible`; HotSpot 25 default is
> `ffff` and R21 `ffffffffft`).
>
> | arm | J27OwnedArgsPrefixRows store,long,vol,param | R21 temp..keep | facts |
> |---|---|---|---|
> | default | ffff 10/10 | ffftffffft 10/10 | SAME |
> | rule + SP liveness | ffff 9/10, ffft 1/10 | ffftffffft | SAME |
> | `IR_INLINE=0` | tttt | ffttffffft | SAME |
> | `IR_INLINE=0` + SP liveness | tttt | ffttffffft | SAME |
> | `IR_INLINE=0` + rule | **fftt** 10/10 | **ffftffffft** (store now false) | SAME |
> | `IR_INLINE=0` + rule + SP liveness | fftt | ffftffffft | SAME |
>
> Before this batch, `IR_INLINE=0` + rule gave `tttt` (`cvm-rpl-2`, 3/3). With the hand-off admitted, the
> local is not the holder either: the hand-off makes the CALLEE own the argument, and `fftt` holds without SP
> liveness.
>
> **Left:**
> - `volatile` / `param` under `IR_INLINE=0`, and `watched`: no exact re-run exists, and the copy is kept
>   while the callee is still in its commit-free prefix. That is design 1 (ARGWIN21-1, weak caller copies).
> - The rule stays opt-in.
> - Host: rerun the A/B arms on 64.177.

> **j27 wave 9 batch 3 (lane W9RPL, 2026-10-29): still NOT FIXED. No sound, small path found. Analysis of the
> `J27OwnedArgsPrefixRows -Dj27.big=true` rows on Windows (`cvm-rpl-2`, `-Xmx96m`, 3 runs per arm).**
>
> | arm | store | long | volatile | param |
> |---|---|---|---|---|
> | HotSpot 25 default / C1-only (`TieredStopAtLevel=1`) | false | false | false | false |
> | HotSpot 25 `-Xint` | true | true | true | true |
> | CratonVM `--nojit` | true | true | true | true |
> | rule on + `IR_INLINE=0` (window ON) | true | true | true | true |
> | + `CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS=1` | true | true | true | true |
> | rule off, `IR_INLINE=0` + SP map liveness | true | true | true | true |
> | rule on + `IR_INLINE=0` + `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` | true | true | true | true |
> | rule on + `IR_INLINE=0`, G1 | true | true | true | true |
> | default (splicing on) | mostly false (noisy) | | | |
>
> What this rules out:
> - The rule does engage: `storeCaller` / `longCaller` print `admitted=true owned=true`.
> - The holder is not the caller's argument copies the rule and the window release.
> - The holder is not the single-pass callee's dead parameter (SP map liveness changes nothing).
> - The holder is not a JIT-frame root scan (still `true` without it).
>
> The `volatile` and `param` rows (refused by the rule) behave identically, so on Windows these rows do not
> isolate the owned-args hand-off at all. The match with HotSpot `-Xint` / CratonVM `--nojit` points at an
> INTERPRETED frame holding the array at the big call. Candidates:
> - the `Row` lambda forwarder or `storeCaller` re-entered through the interpreter for the one big call
>   (`CRATONVM_DBG_DEOPT=1` shows one `not-entrant entry: re-dispatching ...drop([BI)I`);
> - `drop` running interpreted after a supersede.
>
> Next step (not done):
> - attribute the root at the OOME with `CRATONVM_DBG_ROOT_SOURCE=1` (it needs a reader: `native_roots::root_source_of`
>   has none on the OOME path);
> - or count `INTERP_FRAMES` at the big iteration;
> - then compare with the host, where the wave-5 recipe expected `store` / `long` to turn `false`.
>
> Until a holder is named, no code change is justified.

> **j27 wave 9 (lane W9RPL, 2026-10-29): NOT FIXED (the gap stays OPEN). The merge-order step is
> done and verified on Windows; the constant-store rule now acts with dev's window ON and is still
> opt-in.**
> - **Merge order, both steps in order.**
>   1. `deopt::rerun_window_closed_at`'s closure term now asks
>      `owned_args_caller_replay_may_be_exact(code, len, bci, &[])`. That is the superset of every
>      tier's answer, and with the rule off it equals the old `owned_args_caller_replay_is_exact`.
>   2. Only then was the `!CRATONVM_JIT_SERVICE_COPY_WINDOW` term dropped from
>      `owned_args_const_static_stores_enabled`.
>
>   A frame parked before an invoke the rule admits now reaches an exact re-run point, so its
>   caller's copies are kept wherever the hand-off can re-run it. Both switches are read per
>   compile, at the window's compile and at the admission.
> - **Tests** (`cratonvm-jit`, lib; 50 pass under the filters `ir_splice_static_replay j27_oom21
>   osr_service_copy_window owned_args_splice_replay rerun_window`):
>   - new `deopt::j27_oom21_const_static_store_tests::the_window_keeps_a_const_store_invoke_open_under_the_rule`:
>     rule on, bci 0-10 open and 13 closed; rule off, closed from bci 8;
>   - new `...::the_rule_acts_with_the_window_on`;
>   - the old "window on refuses" arm was removed (it asserted the gate).
>
>   `owned_args_splice_replay::j27_w8arg_window_coupling_tests` still passes. Its comment is updated:
>   level 0 of a chain was not widened.
> - **Probe `J27OwnedArgsPrefixRows -Xmx96m -Dj27.big=true` vs HotSpot 25.** Every correctness fact
>   is SAME in every arm and both modes, including the hazard row `handoff-sum 1251451000`. The arms
>   were: default; rule on (window on); rule on + `IR_INLINE=0`; and each of those with
>   `CRATONVM_DBG_FORCE_CALLEE_DECLINE=J27OwnedArgsPrefixRows.drop`.
>   - Engagement: rule on + `IR_INLINE=0` + `CRATONVM_DBG_IR_SLOTS=1` prints `site bci=10
>     J27OwnedArgsPrefixRows.drop([BI)I kind=3 admitted=true owned=true` twice (store, long) and
>     `admitted=false` twice (volatile, param). Before this wave that needed
>     `CRATONVM_JIT_SERVICE_COPY_WINDOW=0`.
>   - The forced-decline lever printed no `forced by` line: `drop` never traps here. Read the decline
>     arm as not engaged.
>   - The `-oome` rows are noisy on Windows. Two runs of the default arm gave all `false`, then
>     `param` `true`, and `CRATONVM_DBG_IR_SLOTS=1` alone flips three of them to `true`. Under
>     `IR_INLINE=0` all four are `true` with the rule on, and equally with the old recipe (rule on +
>     `SERVICE_COPY_WINDOW=0`), so the rule's admission is not where the array is held on this host.
> - **`R21OomArgWindow -Xmx96m`:** facts SAME. `watched-oome true` in every arm (known residue).
>   `store-oome true` under rule on + `IR_INLINE=0` (the gkr table's `IR_INLINE=0` residue,
>   unchanged).
> - **Host must confirm:** the jit lib filters above, then the probe's `-oome` rows on 64.177 with
>   rule on + `IR_INLINE=0` (window ON now, no `=0` needed), 5+ interleaved runs each.

> **j27 wave 8 (lane W8ARG, 2026-10-28): REVIEWED (still PARTLY), PENDING HOST VERIFICATION.**
> The constant-`putstatic` arm of `deopt::prefix_commits_only_rerunnable_stores` was re-read. The
> constant is the instruction right before the store, the push width matches the field's category,
> volatile statics refuse, and the stack is kept in sync. Its tests pin the window gate. Three notes
> for the merge and for triage:
> - **After the dev merge the rule is inert.** `CRATONVM_JIT_SERVICE_COPY_WINDOW` is default ON on
>   dev. dev's hotfix (8335e345ff) counts an owned-args `invoke` with
>   `owned_args_caller_replay_is_exact`, which accepts no static. Merge order:
>   1. Change dev's `rerun_window_closed_at` closure term to `owned_args_caller_replay_may_be_exact(code, len, bci, &[])`.
>   2. Only then drop the `!SERVICE_COPY_WINDOW` term from `owned_args_const_static_stores_enabled`.
>
>   Dropping the gate first is unsound. The window would clear copies at a constant-store prefix
>   that the hand-off re-runs.
> - **A JMM corner the doc does not name.** The re-run's second write is indistinguishable from the
>   first only if this thread performs no release action between the two writes. The callee's
>   abandoned attempt ran in between. `Handoff { exact: true }` means that attempt committed
>   nothing, so it has no monitor op, no volatile write and no invoke. It can still have finished a
>   class's `<clinit>`, which releases the init lock. Another thread that acquired that lock and then
>   wrote the same static would see its write overwritten by the re-run. A legal execution cannot do
>   that. The window is tiny and needs a cross-thread race on a plain static, so this is not filed as
>   a defect. It is one more reason the rule stays opt-in.
> - **The decline is now drivable.** With `CRATONVM_DBG_FORCE_CALLEE_DECLINE=J27OwnedArgsPrefixRows.drop`
>   in the rule-on arm (`..._CONST_STATIC_STORES=1 CRATONVM_JIT_SERVICE_COPY_WINDOW=0`), a trap in
>   `drop` takes the hand-off. The caller is then re-run with its constant store. The `store` /
>   `long` facts must stay SAME as HotSpot. The lever only acts if `drop` traps at all, so check
>   `CRATONVM_DBG_DEOPT=1` for `forced by` lines before reading the row as covered.

> **j27 wave 5 (lane OOM21, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** This builds on dev's
> gkr service-copy window (`CRATONVM_JIT_SERVICE_COPY_WINDOW`, not yet merged into j27) without touching
> it. gkr's measured residue is `store` under `IR_INLINE=0` and single-pass, `watched` everywhere, and
> `twice` single-pass.
> - **`store` row: new opt-in rule `CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES=1`.** The owned-arguments
>   caller rule now also accepts a `putstatic` of a constant (`aconst_null`, `iconst_*`, `lconst_*`,
>   `fconst_*`, `dconst_*`, `bipush` or `sipush`, pushed by the instruction right before it) into a
>   resolved, NON-volatile static. The new code is in `jit/src/deopt.rs`:
>   `owned_args_caller_replay_is_exact_with_statics`, `prefix_commits_only_rerunnable_stores`,
>   `constant_push_words` and `owned_args_caller_replay_may_be_exact`. A re-run writes the same value
>   again. Only another thread racing a plain write can tell, and the JMM lets that race go either way.
>   A volatile static, or any non-constant value, still refuses. The optimizing tier vouches for a static
>   from its built `Op::StoreStatic` nodes (`ir_lower` `Lowerer::own_putstatic_is_plain`, through
>   `owned_args_splice_replay::plain_static_store`). The single-pass tier is NOT wired: `x64/op_invoke.rs`
>   is outside this lane, and it still passes no statics.
> - **The rule stays off while the service-copy window is ON**, which is the window's default. That
>   window counts only `replay_from_entry_commits_nothing` points as re-runnable. So the collector would
>   clear the caller's copies of a frame that a hand-off then re-runs. Dev already has the same hazard
>   today for ARGWIN21-4's fresh-array prefixes (filed:
>   `j27-service-copy-window-ignores-owned-args-rerun-points-FIXED-20261029.md`, with the two-line fix).
> - **`watched` (an invoke in the prefix) is unchanged.** A second `WeakReference` published to a
>   static is not the same write. It still needs design 1 below.
> - Unit tests (`cratonvm-jit`, lib):
>   - `deopt::j27_oom21_const_static_store_tests` (3 tests)
>   - `owned_args_splice_replay::j27_oom21_plain_static_tests` (1 test)
> - Probe: `tools/probes/jit/j27/J27OwnedArgsPrefixRows.java`. Its expected HotSpot 25 lines are in its
>   header: every `-oome` row `false`, plus 10 fact lines.
>
> Host must confirm:
> - `cargo test -p cratonvm-jit --lib j27_oom21_` passes.
> - The probe at `-Xmx96m -Dj27.big=true` prints every fact SAME as HotSpot 25 in every arm of its header.
> - With `CRATONVM_JIT_IR_INLINE=0 CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES=1
>   CRATONVM_JIT_SERVICE_COPY_WINDOW=0`, `store-oome` and `long-oome` turn `false`, and
>   `CRATONVM_DBG_IR_SLOTS=1` shows `site bci=10 ...drop([BI)I kind=3 admitted=true owned=true`.
> - `volatile-oome` and `param-oome` stay `true` (expected residue).
> - `R21OomArgWindow` facts are SAME in both arms.

> **j27 wave 1 (lane FRAMES, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION of nothing new.**
> Re-read against the code: the readers after the transfer are still there
> (`rerun_declined_callee_from_entry`, `service_frameless_callee_trap`, `try_run_callee_handler` in
> `vm/src/jit/helpers/thread.rs`), and `deopt::owned_args_caller_replay_is_exact` still (correctly)
> refuses a prefix with a `putstatic`, an older-object field store or an invoke. None of the three designs
> below fits a no-build wave: design 1 needs a new weak root kind in every collector's root processing
> (ZGC concurrent phases included) and the callee-side horizon in both tiers. Unchanged rows:
> `R21OomArgWindow -Xmx96m` prints `true` off the splice path (HotSpot 25 `false`). Related, landed
> this wave: the spliced-scope local liveness (`CRATONVM_JIT_IR_KEEP_SET_SPLICE_LOCAL_LIVENESS`) is now
> default ON, which narrows the SPLICE path's chain locals only; it does not reach these rows.

# A compiled caller whose bytecode before a call commits keeps that call's reference arguments for the whole callee (OOM21-2)

> **gkr status (2026-10-28): NARROWED, still OPEN.** New "rerun window" (`deopt::rerun_window_closed_at`,
> `CRATONVM_JIT_SERVICE_COPY_WINDOW`, default ON): each tier records where a direct call keeps its service copies
> (`CompiledMethod::service_arg_copies`). At a callee safepoint from which no path reaches an exact re-run point,
> the scan zeroes the caller's copies, because none of the three readers can still run there. The window applies
> only while unsound re-runs raise and no class is redefined.
> `R21OomArgWindow` vs HotSpot (host 64.177.43.150, `cvm-m1`, `-Xmx96m`):
>
> | arm | rows that differ from HotSpot |
> |---|---|
> | default, ZGC / G1 / Gen | `watched` only |
> | `CRATONVM_JIT_IR_INLINE=0` | `store`, `watched` |
> | `CRATONVM_C2_ACCEPT=never` | `store`, `watched`, `twice` |
>
> What is left is the case where the collection happens while the callee is still inside its commit-free prefix.
> There a re-run is still possible, so the window must keep the copy. Closing it needs the copies to become weak with
> a loud refusal (ARGWIN21-1 below), which was not built. Separately, the `Gcd1ArgPinProbe` failure once filed
> under this gap was a different holder (`docs/internal/fixed-bugs/arg-pin-warm-caller-fails-on-dev-FIXED-20261028.md`).

Status: OPEN, external design gap (filed 2026-10-26 while retiring
`docs/internal/jit/jr21-compiled-caller-roots-its-outgoing-arguments-for-the-whole-callee-FIXED-20261026.md`,
which this page splits out). Not a wrong value: an `OutOfMemoryError` HotSpot 25 does not throw, for a
reference argument the callee stops using, on a call that neither tier inlines.
Area: JIT, both tiers (the owned-arguments hand-off), the callee-deopt service (`vm/src/jit/helpers/thread.rs`).

## Rows

`tools/probes/jit/r21/R21OomArgWindow.java` (`-Xmx96m`; HotSpot 25 with `-XX:-UseCompressedOops`):

* `store` (`a = new byte[n]; stat = null; return drop(a, n);`) and `watched` (a `WeakReference` to the
  argument is registered first). HotSpot 25 prints `false` on every collector, C2 and C1-only alike.
* CratonVM: `false` wherever the optimizing tier SPLICES `drop` into the caller
  (`CRATONVM_JIT=force-c2` prints HotSpot's ten rows exactly), `true` where the call stays a call:
  `CRATONVM_JIT_IR_INLINE=0` (both rows), the single-pass tier (`CRATONVM_C2_ACCEPT=never`, both rows),
  and the default tiered run for `watched`, whose OSR driver calls the single-pass `watchedCaller`.

## Why the fixed rows do not cover it

The arguments of a compiled direct call have readers AFTER the transfer that HotSpot has no counterpart
of: the callee-deopt service re-runs the callee from entry on a declined stash
(`rerun_declined_callee_from_entry`) or a frameless trap (`service_frameless_callee_trap`), and runs the
callee's own handler (`try_run_callee_handler`) with them. The owned-arguments hand-off (gcd d9/d on the
single-pass tier, OOM21-1 on the optimizing tier, both default ON since 2026-10-26) removes those readers by
leaving such a trap as the CALLER's frameless deopt, which the caller's caller answers by re-running the
caller from entry -- sound only when the caller's bytecode before the call commits nothing a re-run would
commit twice (`deopt::owned_args_caller_replay_is_exact`; since ARGWIN21-4 a store into an array the prefix
allocated is not a commit). A `putstatic`, a field store into an older object, or any invoke (a
`WeakReference.<init>`) before the call is a commit, so those sites keep their copy.

Declines are not theoretical: `try_resume_trapped_callee` refuses a stash for a redefined callee, a
synchronized one, an unresolvable class, a frame that cannot be rebuilt after a collection during the
build, and so on; and an owned call INSIDE the callee propagates its own declines up as the callee's
frameless trap.

## Designs considered (2026-10-26) and why none was built

1. **Weak caller argument roots + the callee keeps its parameters strong up to its last re-run point**
   (ARGWIN21-1 of `docs/internal/jit-proposals/jit-r21-argwin-proposals-RETIRED-20261026.md`). Sound, and robust to the
   callee body being replaced (every body keeps its own parameters to its own horizon). Needs a new
   WEAK root kind -- updated if the object survives, cleared if not, never traced -- in every collector's
   root processing (ZGC's concurrent phases included) and excluded from the conservative band scan, plus
   the callee-side horizon in both tiers. The retention gain is limited to callees whose last deopt point
   precedes the drop. Large; not started.
2. **The callee's trap record carries its entry arguments** (the stash holds the original reference
   parameters, so a decline re-runs the callee from them). Needs every reference parameter kept live to
   every deopt point of every body (a retention REGRESSION for exactly the shapes this fixes), a frame
   state extension in both tiers, and an answer for frameless traps, which stash nothing.
3. **Hand off only to a callee body that cannot trap at all** (no deopt point, no call, no frameless stub,
   no handler). Unsound as stated: the site binds the callee's entry at compile time, and a later body
   (a recompile reached through the not-entrant entry redirect, or a stale-call rebind) may trap, at which
   point the site has neither the arguments nor a replay-exact caller. Closing that needs a dependency
   that deoptimises such callers' live activations before the callee's entry can redirect, which the VM
   does not have.

## What would close it

Design 1, or a VM in which a callee trap never needs a re-run from entry (every stash resumable
precisely, which is HotSpot's model). Until then the rows above stay `true` off the splice path, and the
single-pass tier's dead PARAMETERS in a non-inlined callee stay published unless the opt-in
`CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS` is on (kept default OFF by triage,
`docs/internal/gc/gch-h3fd-proposal-single-pass-call-site-maps-keep-dead-locals-CLOSED-20261015.md`).
