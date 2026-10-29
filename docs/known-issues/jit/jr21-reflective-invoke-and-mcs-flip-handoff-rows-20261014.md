# Handoff perf rows: reflective `Method.invoke` (`R20PAllocEntryCheck`) and `MutableCallSite` flip (`R20PIndyMutableSite`)

> **j27 wave 8 (lane W8DSP, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (measurement).** No code change,
> on purpose: waves 1, 2 and 5 already left a complete, ordered host recipe (wave 5 block below) and nothing in
> the code read since names the ~85-100 us `--jdk-only` call; a cut without the census / profile numbers would
> be a guess on a 40x gap. Nothing this wave touched (`vm/src/native/jni_platform_rebind.rs`, the
> `jni,resolve` log tag) is on the reflective path. Run wave 5's four steps in order; the first one that shows
> a per-call row (a `(security walk)` line per call, a `basic ... hidden owner` census row ~1 per call, or a
> dispatch-ladder row ~1 per call) names the next build.

> **j27 wave 5 (lane REFLECT, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (measurement).** No code
> change for the perf rows: reading found no new routing defect beyond what waves 1-2 recorded, and an
> unmeasured cut on a ~40x `--jdk-only` vs `--compatible` gap would be a guess. The order for the host run
> is unchanged and complete: (1) `R22InlineReflectLoop` `n = 1` with calls 1-100 / 100-200 / 200-400 timed
> apart (wave 1: a step at ~127 calls is the `Invokers.checkCustomized` re-spin, DEPTH21-1); (2) the same
> under `CRATONVM_DBG_CALLER=1` (one `(security walk)` line per call is the caller-class capture) and once
> under `CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS=1` (wave 2); (3) `CRATONVM_DBG_MH_LINK_CENSUS=1` on
> `R21Reflect4Rows invoke-only`; (4) the frame-pointer profile. Wave 5's parity fix in this area (the
> `Class.getDeclaringClass0` frame, page
> `docs/known-issues/j27-spoofed-member-icce-omits-the-getdeclaringclass0-frame-20261028.md`) touches only
> trace capture of an ICCE / NCDFE raised by `Class`'s nesting natives and cannot move these rows; the
> `fact` lines of `R20PAllocEntryCheck` / `R21ReflectInvoke` must stay equal to HotSpot 25.

> **j27 wave 2 (lane INVOKE, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (measurement).** No change
> for this row. Read again: the compiled constant bind's customization count (`mh_bind_customization_serves`)
> is per call only for handles never customized (fixed for those by `CRATONVM_JIT_MH_BIND_COUNT_RAW`, page
> `jr21-mhbind-constant-bound-and-bindto-20261015.md`); an accessor's target is customized at call 128 and
> then answered from two raw reads, so it does not explain a flat ~85-100 us `--jdk-only` call after 380
> calls. One further reading for the host run: `Method.invoke` asks `Reflection.getCallerClass()` per call
> unless `setAccessible(true)`; its two-frame fast answer (`reflection_caller_class_fast`) is refused
> whenever a reflective-call or served `Thread.run` record is in force, unless
> `CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS=1` (default OFF), and then the whole capture runs.
> Run `R22InlineReflectLoop` once with `CRATONVM_DBG_CALLER=1` (a `(security walk)` line per call is that
> cost) and once with `CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS=1`, before the census and profile below.

> **j27 wave 1 (lane MHINDY, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (measurement).** No code
> change: nothing found by reading explains the `--jdk-only` reflective call (~85-100 us after 380
> calls vs ~6 us `--compatible`, 2.4 us HotSpot, last section); the page's own recipe still decides
> (`CRATONVM_DBG_MH_LINK_CENSUS=1` on `R21Reflect4Rows invoke-only` and
> `tools/probes/jit/r22/R22InlineReflectLoop.java`, both modes, then a frame-pointer `perf record -g`
> of the `n = 1` row). The one reading to add to that run: JDK 25's `Method.invoke` takes
> `DirectMethodHandleAccessor` from the first call (no inflation threshold, JEP 416), so a
> `--jdk-only` cost that falls between call 1 and call ~380 (rather than staying flat) means a
> customization / tier-up transition (`Invokers.checkCustomized` at 127 calls re-spins the
> accessor's form as a hidden `LambdaForm$MH`, DEPTH21-1) -- time calls 1-100, 100-200, 200-400
> separately before profiling.

Status: PENDING HOST MEASUREMENT (JIT round 21, lane reflect4, wave 14: the `R20PAllocEntryCheck` reflect rows now have a per-call census, `CRATONVM_DBG_MH_LINK_CENSUS` (REFLECT4-21-3), which names the step each compiled `invokeExact` / `invokeBasic` / `linkTo*` link of a `Method.invoke` took; its numbers decide the next build, see "Round 21 wave 14 (lane reflect4)"). Previous status: PARTLY VERIFIED (round-21 host check, 2026-10-15). The `R20PIndyMutableSite` stable rows are fixed (with lane mcs): w3i mcs-stable 11 ns, vcs-stable 11, virt-stable 17 (from ~172 / 173 / 218), mcs-flip 115 (171 on w17s), `R21ReflectMcsStable` 11-12 (w9ab-w3i.txt); facts SAME (`R21ReflectMcsStable` 11, `R21McsGuardSplice` 17, fz21-w3m-R21.txt). Still PENDING HOST MEASUREMENT: the `R20PAllocEntryCheck` reflect rows -- `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST=0` is flat on w3a (reflect-static2 8110 vs 8432, reflect-static-call 15257 vs 14136, reflect-virtual-call 13966 vs 14760; w3ab-w3a.txt), and the perf profile that names the microseconds is in no round-21 log (w3m reflect-static2 still 3994 ns, HotSpot 2). Previous status: FIXED IN CODE (JIT round 21, lane reflect), PENDING HOST VERIFICATION -- the `R20PIndyMutableSite`
stable rows (two defects found by reading: a retarget back left the site's newest guard cell naming the target it
had left, and the one optimizing-recompile request a guarded site makes was spent when the tier answered "already
queued" or before a later retarget; see "Round 21 fix (lane reflect)" below). The `R20PAllocEntryCheck` reflect rows
stay PENDING HOST MEASUREMENT: one per-call cost cut (`CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST`) and one caching fix
are in, plus the evidence line that names the hidden `LambdaForm$MH` hop (DEPTH21-1); the 12 us is not explained by
anything found by reading, so a profile decides. (Lane depth's earlier status: the w11d figures are partly OBSOLETE
on 53fb817818; DEPTH21-1 / DEPTH21-2 are structural.)

Re-land (20261026): merge `624fae0b49` had dropped this round-21 code from dev; it is back on dev since `44f0382076` (formatted 3-way re-land, `docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md`). The status above describes the code again; its host verification is recorded there.

Area: `native-builtins/src/lang_invoke.rs` (`mh_linked_invoker`, `mh_invoke_basic`), `vm/src/jit/helpers.rs`
(`mh_compiled_link_redirect`), `vm/src/runtime/invokedynamic.rs` (`fill_direct`, `publish_mutable_guard`),
`jit/src/ir.rs` (`try_indy_mutable_guarded_call`).
Found by: JIT round 21 lane depth (handoff r20 "Round-21 performance targets").

## Row `R20PAllocEntryCheck` reflect-static / reflect-virtual (handoff 23858 / 24860 ns, HotSpot 13 / 14)

The handoff table is "from the w11d A/B". After it, round 20 landed two cuts for exactly this row:
`CRATONVM_MH_LINKER_HIT_SKIPS_ROOT_REFRESH` (wave 17, 62e28a0d71) and `CRATONVM_JIT_MH_COMPILED_LINK` (wave 19,
ad9c66cd5e); the round record measured 20.6 -> 12.6 us static, 20.3 -> 11.9 us virtual on w17s
(`docs/internal/fixed-bugs/jr20-reflective-method-invoke-runs-a-native-door-chain-per-call-CLOSED-20261012.md`,
residual REFL21-1 in `jit-r20-wave20-residual-proposals.md`).

Per call (both modes; `CRATONVM_JDK_REFLECTIVE_INVOKE` runs JDK 25's `Method.invoke` in both), by reading on this
base, with the compiled link engaged:

1. `Method.invoke` (compiled Java): `Reflection.getCallerClass()` native (two-frame fast answer), `checkAccess`.
2. `DirectMethodHandleAccessor.invokeImpl`'s `invokeExact`: `mh_compiled_link_redirect` (Linker shape) --
   `mh_compiled_link_target` builds the linker-table key (a `format!` plus `class_keyed`, a shareability check)
   per call, then `mh_bind_direct_call` into `Invokers$Holder.invokeExact_MT`.
3. `invokeExact_MT` -> `invokeBasic(adapter, ..)`: the adapter's `form.vmentry` is a HIDDEN spun `LambdaForm$MH`
   class; the compiled link refuses it (by-name resolution), so the `invokeBasic` native door runs
   (`mh_invoke_basic` -> `mh_real_lf_entry_fast`: unboxing plus an `interpreter::execute` re-entry).
4. The adapter form unboxes, `invokeBasic` on the direct handle (linked), `linkToStatic` (linked) -> the target.

The one remaining native door + VM re-entry (step 3) and the key formatting (step 2) are not 12 us by any per-item
cost this repo has measured (a door ~0.5-1 us, a re-entry ~0.6-0.7 us). So either the w17s figure is no longer the
number on this base, or something in the chain still falls to a slow arm. No routing defect is visible by reading.

### What decides (host)

* `R20PAllocEntryCheck`, `--jdk-only` and `--compatible`, default vs `CRATONVM_JIT_MH_COMPILED_LINK=0`, interleaved.
  If `time reflect-*` is near 12 us and `=0` near 20 us, the rest is DEPTH21-1's hop; profile with
  `sudo -n perf record -e cpu-clock -g` + `perf report --sort symbol`. Expected at the top: `mh_invoke_basic`,
  `mh_real_lf_entry_fast`, `interpreter::execute`, `mh_compiled_link_target`. If `LambdaForm.interpretWithArguments`
  / `interpretName` show up, the adapter form's `vmentry` is not used: a defect, file it.
* `CRATONVM_DBG_CALLER=1`: only `(fast)` lines expected; a `(security walk)` per call is a defect.

## Row `R20PIndyMutableSite` mcs-flip (handoff 17760 ns, HotSpot 24)

OBSOLETE as a figure: r20 wave 17 (e209100986, `CRATONVM_JIT_INDY_DIRECT_RECENT_TARGETS`) found the cause -- after
eight retargets in one epoch `fill_direct` stopped refilling and half the batches paid the generic
`MethodHandle.invoke` -- and the record measured 4401 -> 171 ns (HotSpot 30) on w17s
(`docs/internal/fixed-bugs/jr20-pstr3-code-point-latin1-static-init-mcs-flip-rows-CLOSED-20261012.md`).

What is left, by reading: the compiled guard (`try_indy_mutable_guarded_call`) compares against ONE planned target
(the newest `IndyMutableGuardCell` at compile time), so the batches holding the other target take the `IndyBridge`
miss edge (bridge + `jit_indy_linked_direct_leaf` + `jit_invoke_dispatch_body`, ~170-250 ns). The record's recheck
also shows the STABLE rows at ~172 ns, the bridge's cost, not "a few ns": if that holds on this base, the guarded
call is not engaging on stable sites either. Check that first:

* `R20PIndyMutableSite`, default vs `CRATONVM_JIT_INDY_MUTABLE_GUARD=0`, `CRATONVM_DBG_JITC=1`: the
  `[ir] indy MUTABLE guarded call at bytecode pc` line for `g.Drive.kernelM` must appear and `mcs-stable` must be
  well below `=0`. If `mcs-stable` equals `=0`, the guard's hit path is not taken -- a defect to file. Candidates by
  reading: the optimizing recompile request (`request_constant_recompile`) is one per site and the first cell
  spends it, so a body compiled before the cell existed may never be recompiled with the plan; or
  `has_frame_state_at(pc)` refuses the site.
* The flip residual is DEPTH21-2 (a two-target guard).

## Round 21 lane indylate (host A/B on the round head)

`R21ReflectMcsStable` mcs-* stayed ~260 ns and `R20PIndyMutableSite` mcs-stable / vcs-stable / virt-stable
~370-400 ns under every arm: the guarded recompile requests below were answered `Enqueued` and then SKIPPED by the
acceptance gate's refusal memo. Fixed in code on
`jr21-indylate-indy-recompile-requests-skipped-by-the-acceptance-memo-20261014.md`; verify the stable rows with it.

## Round 21 fix (lane reflect)

### `R20PIndyMutableSite` stable rows: why the guard never served them

The compiled guard (`IrBuilder::try_indy_mutable_guarded_call`) is planned ONCE, at compile time, from the site's
newest `IndyMutableGuardCell` (`invokedynamic::jit_indy_mutable_guard_target`). Two routing / caching defects, by
reading (`vm/src/runtime/invokedynamic.rs`):

1. **A retarget back left the newest cell stale (fixed, unconditional).** `JitIndyGenericSite::fill_direct`'s
   recent-memo arm (r20 wave 17, `CRATONVM_JIT_INDY_DIRECT_RECENT_TARGETS`) swaps a displaced memo back in on a
   retarget back -- and returns before `publish_mutable_guard`, so `IndyDirectState::mutable_cell` kept naming the
   target the site had just LEFT. `R20PIndyMutableSite` runs `mcs-a`, `setTarget(B)`, `mcs-b`, `setTarget(A)`
   before its stable rows: an optimizing compile after that planned `cur == B ? addB : bridge` while the site held
   A, so every call took the bridge (bridge + `jit_indy_linked_direct_leaf` + `jit_invoke_dispatch_body`: the
   ~170 ns the record shows for mcs-stable, with or without the recent-targets switch). Fix:
   `IndyDirectMemo::cell` remembers the cell published for the memo's target (`publish_mutable_guard` now returns
   it); the swap-back stores it as the site's newest cell again (no mint, no recompile request).
2. **The single recompile request could be spent before any guard could be planned
   (`CRATONVM_JIT_INDY_MUTABLE_GUARD_BRIDGE_RECOMPILE`, new, default ON, `=0` off).** The first cell asks for the
   method's C2 recompile through `request_constant_recompile`, one shot per site: the flag is set before the tier
   answers, and the tier answers `AlreadyQueued` whenever the C1 publish already queued the C1 -> C2 upgrade --
   the common case, since the first bridge fill runs in that very C1 body. If that queued compile read the site
   before the cell existed, the optimizing body has no guard and nothing asks again. Likewise a retarget after the
   optimizing compile (a site that settles on a new target) mints a new cell but never a recompile. Now a bridge
   call that the direct leaf serves while the site's newest cell plans exactly that call (same VM, same leaf, the
   cell's target slot holding the very handle the call site holds) counts; at the 1024th such call and every power
   of two after it the site asks `TieredCompilationManager::request_indy_mutable_guard_recompile` (new; the same C2
   retry door, answer returned), until 4 requests were `Enqueued` (an `AlreadyQueued` answer is asked again at the
   next power of two). Cost on the bridge: one atomic load; on a guarded site a slot read, a compare and a relaxed
   add (`JitIndyGenericSite::note_guard_bridge_hit`, called by `jit_indy_linked_direct_leaf` after it released the
   memo).

Not changed: a site flipping between two targets still guards one (DEPTH21-2's two-target guard is what would
serve `mcs-flip`; the bounded asks above keep it from recompiling per flip). Not covered: the same one-shot spend
for a CONSTANT call site (`request_constant_recompile` itself), filed as
`jr21-indy-constant-recompile-request-spent-on-already-queued-FIXED-20261015.md`.

### `R20PAllocEntryCheck` reflect rows

* `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST` (new, default ON, `=0` off; `types` `NativeFlags`
  `jit_mh_compiled_link_pair_first`): `lang_invoke::mh_compiled_linker_pair` -- run on every compiled
  `invokeExact` link, i.e. once per `Method.invoke` (`DirectMethodHandleAccessor.invokeImpl`) -- re-checked
  `bound_method_type_shareable(site)` per call: a descriptor parse into `String`s and a class-name lookup plus a
  synthetic-stub check per component. The door (`mh_linked_invoker`) is the only writer of the
  `linker:invokeExact:<site>` rows and files one only after that very check passed, so a row found is the proof;
  ON, the check is skipped and the table answers.
* Caching fix (unconditional), `vm/src/jit/helpers.rs` `mh_link_leaf`: a member refused because it is a static of
  a class not initialized yet was memoised as refused (`1`) for the whole collection epoch, though that refusal
  lifts as soon as the door serving the call initializes the class. Now that one refusal is left unfiled
  (`MhCompiledMember::refusal_may_lift`, new) and the next call asks again.
* Evidence (cold, once per member per epoch): `CRATONVM_DBG_JITC=1` prints
  `[mh-compiled-link] door kept for <class>.<name><desc> kind=<k>[ (hidden owner)]` for every compiled link
  refused (`MhCompiledMember::owner_is_hidden`, new). On `R20PAllocEntryCheck` a `(hidden owner)` line naming a
  `java/lang/invoke/LambdaForm$MH/0x...` class is DEPTH21-1's hop: JDK 25 builds the accessor's target with
  `MethodHandles.dropArguments` + `asType` (`MethodHandleAccessorFactory.makeSpecializedTarget`) and customizes it
  (`Invokers.checkCustomized`, after 127 calls); a customized form's `vmentry` is always a spun hidden class.

Why DEPTH21-1 is not built here: calling a hidden body directly from `mh_compiled_link_redirect` is easy, but
every service around a compiled call resolves the callee BY NAME from the info (`callee_has_exception_table_uncached`,
`try_resume_trapped_callee`'s own-source and identity checks, `stash_reachable_from_site`, the dispatch ladder's
static tail `invoke_static_or_native_prepared`), and the class manager's by-name finds do not answer a hidden
class (`find_bootstrap_class_by_name` filters `hidden` explicitly). A deopt inside the directly called hidden body
would leave its stash unclaimed (an orphan, re-run from entry) or hand the ladder a name no loader defines. The
by-id route is proposal REFLECT21-1 (`docs/internal/proposals/jit/tier3/jit-r21-reflect-proposals-20261014.md`).

### HOW TO VERIFY

* `R21ReflectMcsStable` (`C:\craton\jr21-ssh\probes\`): arms default,
  `CRATONVM_JIT_INDY_MUTABLE_GUARD_BRIDGE_RECOMPILE=0`, positive control `CRATONVM_JIT_INDY_MUTABLE_GUARD=0`, both
  modes. Expected: default `mcs-fresh-stable`, `mcs-flipped-back`, `mcs-retargeted-late` a few ns (well below the
  control's bridge cost); `=0` of the new switch leaves `mcs-retargeted-late` (and possibly `mcs-fresh-stable`) at
  the control's cost; facts equal HotSpot 25 in every arm. `CRATONVM_DBG_JITC=1`: `[ir] indy MUTABLE guarded call
  at bytecode pc` for `g.Drive21.k1/k2/k3`, `[indy-mutable] ... recompile Enqueued` lines (at most 4 per site).
* `R20PIndyMutableSite`: `mcs-stable` / `vcs-stable` / `virt-stable` should drop from ~172 / 173 / 218 ns towards a
  few ns; `mcs-flip` must not be slower than before (171 ns on w17s); facts unchanged.
* `R21ReflectInvoke` and `R20PAllocEntryCheck`, arms default vs `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_FIRST=0` vs
  `CRATONVM_JIT_MH_COMPILED_LINK=0`, interleaved, both modes; `fact` lines equal HotSpot 25 in every arm.
  `R21DepthReflectAndIwa` `nested-reflect-400-warm` stays 400. Then the profile this page asked for
  (`sudo -n perf record -e cpu-clock -g`, `perf report --sort symbol`), and `CRATONVM_DBG_JITC=1` once to read the
  `(hidden owner)` lines; whatever tops the profile is the 12 us.

## Round 21 lane mcs

The stable rows' residual (~170 ns after lanes indylate / indylate2) was the guarded call's hit edge lowered to the
blind `jit_invoke_dispatch`: the inline planner's spliced-pc sweep dropped the direct row and IC pair of the guarded
pc, which the `0xba` arm never splices. Fixed (plus a spliced hit edge, `CRATONVM_JIT_INDY_MUTABLE_GUARD_SPLICE`) on
`jr21-mcs-guarded-indy-call-lost-its-direct-row-20261014.md`; verify mcs-stable / vcs-stable / virt-stable there.

## Round 21 wave 14 (lane reflect4): the compiled-link census (REFLECT4-21-3)

`reflect-static2` is still 3994 ns against HotSpot's 2 (w3m), and nothing in any round-21 log says which step
of the three links a `Method.invoke` makes (`invokeExact` -> the linker, `invokeBasic` -> the adapter's
`form.vmentry`, `linkTo*` -> the target) falls to its door, or how often. Lane reflect's
`[mh-compiled-link] door kept` line fires once per member per epoch, so it cannot tell a hop that costs every
call from one that fires once; lane reflect3's memos measured 100x slower on one row with no explanation.

### What is in (`CRATONVM_DBG_MH_LINK_CENSUS`, new DBG row, `=1`)

* `vm/src/jit/helpers.rs` `mh_compiled_link_redirect` / `mh_compiled_link_target`: every compiled
  signature-polymorphic link is counted by outcome, per call, into a per-thread `JitCounterBlock` slot (no new
  static): which step refused it (the linker pair not filed, a decode refused, the shape or basic types
  disagree, the leaf refused -- for `invokeBasic` split into "hidden owner", the `LambdaForm$MH` hop, and
  other), or that it was served, split by whether the leaf's compiled entry was already memoised where
  `mh_bind_direct_call` looks first (served direct) or not (the dispatch ladder: compile, interpret or
  native). The reason is set before each step (`why`), so the refusal is named by construction, not inferred.
  Off: one flag-snapshot load per link.
* Printed at exit by `vm-cli` (`maybe_dump_shutdown_reports`, both shutdown arms), one line per non-zero row:
  `[cratonvm]   basic (invokeBasic): vmentry leaf refused, hidden owner (door: the LambdaForm$MH hop,
  DEPTH21-1): <n>`.

### What decides (host)

* `R21Reflect4Rows invoke-only` (only `reflect4-invoke-static2` and `-virtual` run), both modes, under
  `CRATONVM_DBG_MH_LINK_CENSUS=1`, arms default and `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=0`; divide each row
  by `2 x fact w14-invoke-calls` (530000 per row).
  * `linker ... served` and `basic ... served` and `linkTo* ... served` ~1 per call each, all "memoised": the
    links are not the cost; profile `mh_bind_direct_call`'s callees (`perf report --sort symbol`).
  * A "no compiled entry memoised (dispatch ladder)" row ~1 per call: the leaf's body is entered through the
    ladder every call -- its DISPATCH_CACHE fill is refused; the next build is that fill (name the leaf with
    `CRATONVM_DBG_JITC`).
  * `basic ... hidden owner` ~1 per call in the default arm: `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN` is not
    engaging; REFLECT4-21-2 (by-id template) or the hidden leaf's own refusal is next.
  * `linker ... no linker pair filed` ~1 per call: the door never files the pair for this site (caller class or
    descriptor mismatch); that is the next fix.
* Same census on `R20PAllocEntryCheck` and `R21ReflectInvoke`, default vs `CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO=1`
  / `_PAIR_MEMO=1` (read-only use of those switches): the memos' 100x loss should show as a row moving from
  "served, memoised" to a door row.

## Retirement of the r22 inline book (2026-10-25): the per-call cost, measured on a loop callee

Found while reading `R22InlineReoffer`'s `hidden-run` row (jdk-only 29 ns per loop iteration, `--compatible` 2,
HotSpot < 1). It is not the hidden-class self-reference: an ordinary class through `Method.invoke` measures the
same. `tools/probes/jit/r22/R22InlineReflectLoop.java` calls `run(n)` (a loop of `n` iterations of two spliced calls) reflectively, 200 warm-up calls
then 9 x 20 timed ones, ns per iteration (host 64.177.43.150, dev `16c3e96e27` plus the r22 inline
close):

| `n` | `--jdk-only` | `--compatible` | HotSpot 25 |
|---|---|---|---|
| 1 | 100 935 | 6 143 | 2 446 |
| 100 | 837 | 54 | 27 |
| 5 000 | 12 | 1 | 0 |

So the compiled callee runs at full speed once entered (the `n = 5 000` row is ~60 us of call over 5 000
iterations); what is left is the reflective call itself after 380 calls: ~85-100 us under `--jdk-only`, ~6 us
under `--compatible`, ~2.4 us on HotSpot (not yet inflated at that count). A
`MethodHandle.invoke` of the same hidden-class method is 8 ns per iteration in both modes. This page owns the
number; nothing was changed for it.
