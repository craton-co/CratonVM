# Reflective calls trail HotSpot by microseconds (2026-11-16)

A `Method.invoke` or a JDK `Proxy` call still costs microseconds where HotSpot 25 needs nanoseconds.
Every row below gives the same answers as HotSpot.

Probes: `tools/probes/ProxySplit.java` (one layer per row, see its header), `tools/probes/MiProbe.java`
(one `M.invoke` on a `static final Method` in one long loop), and `FrozenRowsBench proxy`
(`tools/probes/FrozenRowsBench.java`, archived from the tree in `f4a96dc1d3`).

## Rows

Linux host (16 cores, shared, load 9-18), release build (`lto=off`, 16 codegen units, each binary in its
own target dir), `--real-jdk`, last rep of each run, 3 interleaved rounds (round 2 ran under a load spike).
"base" is dev `5b2dfa3fa0` plus the `lambda_proxy_satisfies` change below; "gate" adds the cold-arm
change below.

| Row | base | gate | HotSpot 25 |
| --- | --- | --- | --- |
| MiProbe | 2.3-4.4 us | 2.0-4.0 us | 3.8 ns |
| ProxySplit `invoke` (lambda) | 3.1-4.7 us | 2.8-4.5 us | 6.7 ns |
| ProxySplit `invcls` (class) | 1.8-3.3 us | 1.6-3.4 us | 7.5 ns |
| ProxySplit `prefl` (proxy) | 2.7-6.7 us | 2.5-6.3 us | 9.9 ns |
| ProxySplit `mhv` (bind hop) | 1.1-2.0 us | 1.05-2.0 us | 18 ns |
| FRB `proxy` (last of 3 reps) | 3.0-5.4 us | 2.9-11.0 us | 1.1 ns |

The gate change moves no row on this host (see below for why). On the Windows host (profsym build) the
same rows were 0.2 us (MiProbe, bimodal on some runs) and 3.5-6.8 us (proxy rows).

`--compatible` could not be measured on that dev (the launcher exited before the VM started); it runs
on current dev, see the latest update below.

## What already works

dev folds the chain HotSpot folds when `Method.invoke` is spliced into the loop's optimizing body:

- the `static final Method`'s trusted and `@Stable` fields (`methodAccessor`, `callerSensitive`, the
  accessor's `target`) are constants, and the `asType`'d `target.invokeExact` compiles as its guards
  and the member's plain call (`invoke_kind 7`);
- `Reflection.getCallerClass()` inside a spliced `Method.invoke` is the splicing frame's class
  mirror, skipping the frames HotSpot's security walk skips;
- `Reflection.getClassAccessFlags` of a folded class is a constant.

When that body runs (MiProbe's steady state on Windows), no sample lands in `getCallerClass`, in the
`invokeExact` bind, or in `getClassAccessFlags`.

## Why the rows are still slow

The reflective chain keeps (or falls back to) single-pass bodies, and then every call pays it as
runtime helpers: the `invokeExact` bind hop (`mh_constant_bind_call` -> `mh_bind_direct_call`), the
native `Reflection.getCallerClass` stack walk, a native `Class.isAssignableFrom` from
`DirectMethodHandleAccessor.checkReceiver`, and `class_id_from_mirror` under a shared lock.

What keeps the optimizing bodies out is the acceptance gate's cost model
(`CompileRecord::added_ns_per_execution`): a blind dispatch inside a splice costs ~171 ns against
~4 ns credited per spliced frame, and the gate refuses a body that nets above zero. Measured with the
JIT trace (`ProxySplit invoke`):

- **Linux.** `ProxySplit.invoke`'s optimizing body is accepted, but its plan drops the
  `Method.invoke` splice, so `Method.invoke` runs as its own body. That body is REFUSED for one blind
  call to `Method.isCallerSensitive()` (private, hot, no compiled entry when the splice is built:
  its own callee `Reflection.isCallerSensitive` is refused as an `ldc` of a non-numeric constant).
  `DirectMethodHandleAccessor.checkReceiver` is REFUSED for two blind `AbstractStringBuilder.appendNull`
  calls inside `StringBuilder.append` bodies spliced into its exception-message arm: that arm is
  cold, but the spliced bodies' code is not priced by the weight of the arm they are called from.
- **Windows.** The loop's optimizing bodies trap on a spliced `Integer.valueOf` branch pruned from a
  frozen callee profile (4 traps, then the per-bci de-spec drops the prune). A callee eviction then
  leaves a blind `AccessibleObject.slowVerifyAccess` call in the spliced `verifyAccess`, and the gate
  refuses the last recompile, so the loop keeps its single-pass body.

MiProbe's optimizing body (Windows) is ~40x HotSpot for another reason: the `Integer` box and the
varargs `Object[]` are allocated per call (HotSpot scalar-replaces both), and on ZGC the access
cache's `WeakReference.refersTo(caller)` reads `referent` through the checked `jit_getfield` helper,
by design (the VM names the inline compare-only row only when ZGC's resurrection gate cannot arm).

## Update 2026-11-16 (later): the bound-site cache is back; the next blocker is the splice

The Linux refusals above were not cold-arm pricing. `aa2e56b5ee` (the static-final `Method.invoke`
fold) limited the call lowering's inline-cache cascade to invoke kinds 0 and 2, which turned off the
bound-site cache nio21 w15 gives a statically bound instance call with no compiled callee. Every
private callee (`Method.isCallerSensitive`, `AbstractStringBuilder.appendNull`, `ArrayList.grow`) was a
by-name dispatch on every call, and the gate refused its caller. With kind 1 admitted again
(`ir_ic_cascade_admits_kind`), the JIT trace of `ProxySplit invoke` shows no blind kind-1 dispatch and
no cost refusal: `Method.invoke`, `DirectMethodHandleAccessor.invoke` / `invokeImpl` /
`checkReceiver` are all accepted.

The rows do not move (3 interleaved rounds, `invoke` 2.41-2.51 us before, 2.39-2.42 us after). The
folds that remove the chain apply only when `Method.invoke` is spliced into the loop with the
`static final Method` as a constant, and `ProxySplit.invoke`'s plan drops that splice:

- with `Integer.valueOf` spliced the build stops in its pruned branch (`IrBuilder::build refused`,
  combined pc 706), and the planner retries without it;
- without it, the build stops at combined pc 166: `Reflection.isCallerSensitive(Method)` inside the
  spliced `Method.isCallerSensitive()`, the once-per-`Method` arm (`callerSensitive == 0`), has no
  call row (its callee is refused as an `ldc` of a non-numeric constant and gets no direct entry or
  late cell), so the build bails and the planner drops the `Method.invoke` splice.

## Update 2026-11-16 (late): `Method.invoke` is spliced; the accessor is the next blocker

On current dev (`253801adb9`) the JIT trace of `ProxySplit invoke` shows `ProxySplit.invoke`'s
optimizing body accepted with `Method.invoke` spliced into the loop (16 callee bodies, no
`ir-splice-rebuild`) in both modes. Rows on the Linux host, 3 interleaved rounds, last rep, "base" dev
`253801adb9`, "fix" adds the dead-row change below:

| Row | `--real-jdk` base | `--real-jdk` fix | `--compatible` base | `--compatible` fix | HotSpot 25 |
| --- | --- | --- | --- | --- | --- |
| MiProbe | 111-126 ns | 110-111 ns | 114-115 ns | 116-124 ns | 3.8 ns |
| ProxySplit `invoke` | 1.54-1.56 us | 1.57-1.61 us | 1.55-1.58 us | 1.57-1.60 us | 6.7 ns |
| ProxySplit `invcls` | 0.82-0.84 us | 0.81-0.82 us | 0.82-0.86 us | 0.83-0.86 us | 7.5 ns |
| ProxySplit `prefl` | 1.57-1.60 us | 1.58-1.60 us | 2.23-2.32 us | 1.59-1.62 us | 9.9 ns |
| ProxySplit `mhv` | 1.02-1.06 us | 1.04-1.11 us | 1.04-1.07 us | 1.04-1.06 us | 18 ns |
| FRB `proxy` (ns/op, 3 reps) | 1571-1690 | 1577-1625 | 2402-2812 | 1589-1631 | 1.1 ns |

`FrozenRowsBench facts`, `AsTypeEdge` and `R21ReflectInvoke` give HotSpot's answers on both binaries
in both modes.

What still runs as calls in the loop body, from the trace:

- `MethodAccessor.invoke` (`DirectMethodHandleAccessor.invoke`): guarded on the profiled accessor class
  but refused as a splice for its exception table (`callee-exception-table`), so the whole
  `invokeImpl` / `checkReceiver` / `invokeExact` chain runs as a call per iteration. This is most of
  the remaining microsecond.
- `Method.isCallerSensitive()`: a bound call. Its callee `Reflection.isCallerSensitive(Method)` is
  refused for an `ldc` of a non-numeric constant, and is declined a compiled entry
  (`eager-callee-chain-cold`).
- `Reflection.getCallerClass()`: a `Bridge` native (`native-shadow`), so it is not spliced in this plan.

## Update 2026-11-17: `Method.invoke` within 1.6x of HotSpot on G1

Linux host, 3 interleaved rounds, last rep, ns/op. "base" is dev `9ca9d11ccc`; "dev" adds the
changes listed under "Fixed here" below. CratonVM's default collector is ZGC.

| Row | base | dev (ZGC) | dev `--compatible` | dev (G1) | HotSpot 25 |
| --- | --- | --- | --- | --- | --- |
| ProxySplit `invoke` | 1648-1660 | 33.2-34.2 | 33.5-34.0 | 14.3-14.4 | 7.2-10.9 |
| ProxySplit `invcls` | 834-854 | 32.8-33.0 | 32.5-33.6 | 13.4-16.1 | 7.5-14.7 |
| ProxySplit `mhc` | 1113-1126 | 19.1-38.7 | 6.0-12.7 | 8.6-9.4 | 0.3 |
| ProxySplit `prefl` | 1666-1692 | 126-147 | 1216-1228 | 31-32 | 3.2-5.3 |
| ProxySplit `mhv` | 1096-1119 | 1071-1082 | 1077-1093 | 1169-1890 | 17-21 |
| MiProbe | 113-243 | 31.7-61.8 | 31.8-62.3 | 12.4-21.1 | 2.9-4.5 |

`FrozenRowsBench facts`, `AsTypeEdge` and `R21ReflectInvoke` give HotSpot's answers on both binaries in
both modes (HotSpot itself answers `R21ReflectInvoke`'s two null-receiver facts with `null` on some
runs, its fast-throw path, and with the helpful message on others).

The `invoke` loop now has no allocation and two calls (the target lambda's `price`, and the
`refersTo` cache test's cold miss call). What is left:

- **ZGC field loads.** Fixed in the later update below: the per-call helper was the access cache's
  compare-only `referent` read, not the three ordinary fields.
- **The `price` call.** The folded handle's member call is built at build time, so it is an interface
  call, never a splice.
- **`prefl` (a JDK `Proxy`).** The proxy body's `static final Method` is read inside a spliced body,
  where the VM mints its constant folds without a thread, so the accessor's `target` handle gets no
  row and `invokeExact` stays a runtime bind until a recompile finds one. The row's survival depends
  on GC timing (rows are keyed by address and collection epoch). `--compatible` misses it every time.
- **`mhv`** (a non-constant handle) is the runtime bind, unchanged.

## Update 2026-11-17 (later): ZGC no longer pays a helper per call

On ZGC (the default collector) the remaining helper call was the access cache's `refersTo`: a heap
whose referent read gate may arm got no compare-only row for `Reference.referent`, so every compact
`WeakReference` read went through `jit_getfield`. The row is now named on every collector, and the
lowering reads it inline when every compare is against one value it tests for null first (see "Fixed
here"). 3 interleaved rounds, quiet rounds shown, ns/op; "base" is dev `e6146b6b96`:

| Row | base | now (ZGC) | now `--compatible` | now (G1) | HotSpot 25 |
| --- | --- | --- | --- | --- | --- |
| ProxySplit `invoke` | 33.1-34.3 | 13.9-14.1 | 13.8-14.2 | 14.5 | 7.0-7.4 |
| ProxySplit `invcls` | 33.0 | 13.7-13.8 | 13.6 | 13.3 | 7.8-10.1 |
| MiProbe | 31.7-32.2 | 12.1-12.2 | 12.1-12.3 | 12.2 | 2.8-3.2 |
| ProxySplit `prefl` | 124-132 | 94-96 | 1158-1182 | 30.8 | 3.2-5.0 |

From the first table on this page (1648-1660 ns against HotSpot's 7.2-10.9), ProxySplit `invoke` is
now about 2x HotSpot on the default collector. `FrozenRowsBench facts`, `AsTypeEdge` and
`R21ReflectInvoke` give HotSpot's answers in both modes.

## Update: non-constant handles and the proxy row

A `MethodHandle` read from a non-final static now folds like a `static final` one, behind an identity
guard, and the proxy row's handle rows no longer wait for a recompile (its handler splice still did; see
the next update). 3 interleaved rounds, last rep, ns/op; "base" is dev `a4d72a42b0`:

| Row | base (ZGC) | now (ZGC) | base `--compatible` | now `--compatible` | base (G1) | now (G1) | HotSpot 25 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ProxySplit `mhv` | 713-739 | 9.6-10.1 | 731-746 | 5.8-6.6 | 848-868 | 13.8-14.9 | 15-18 |
| ProxySplit `prefl` | 31 | 31-32 | 819-826 | 30-39 | 927-947 | 43 | 3.2 |
| ProxySplit `invoke` | 14.1-14.5 | 13.9-14.4 | 13.6-14.5 | 13.7-14.2 | 14.4-14.5 | 14.2-14.7 | 7.3-9.9 |
| ProxySplit `mhc` | 9.6-10.1 | 9.9-10.0 | 5.8-6.2 | 5.9-6.1 | 14.0-15.4 | 15.0-16.7 | 0.3 |
| MiProbe | 12.2-12.3 | 12.3-12.8 | 12.2-12.3 | 12.2-12.5 | 12.3-12.5 | 12.2 | 2.9-3.1 |

`mhv` now runs at or under HotSpot's time in all three configurations. The base `prefl` rows show the
old fragility: G1 read 31 ns on the previous base and 927-947 ns on this one. `FrozenRowsBench facts`,
`AsTypeEdge` and `R21ReflectInvoke` give HotSpot's answers in both modes.

## Update: the proxy's handler splice no longer waits for a trap

`prefl` was still bimodal on every binary: a run settled at 31-43 ns or at 0.8-1.4 us, and which one
depended on timing (dev `2c68237ab5` was slow in 2 of 3 G1 runs, and slow in every G1 run once merged
with the update above). The slow runs kept the proxy's `InvocationHandler.invoke` call. The optimizing
tier splices a profiled call only after `MIN_GUARDED_RECEIVER_EVIDENCE` (32) receiver observations, and
`$Proxy0.price`'s first optimizing compile saw 28-31: the invoke doors' profile coalescing records only
every 32nd call of a (site, receiver) run, and counted each record as one call, so the count grew about
1000 times slower than the calls. A fast run was one where an unrelated uncommon trap recompiled the
method later. A sampled record now counts every call it stands for, so the bar means 32 calls again.
3 interleaved rounds, last rep, ns/op; "before" is the merge of the update above into dev `2c68237ab5`:

| Row | before (ZGC) | now (ZGC) | before `--compatible` | now `--compatible` | before (G1) | now (G1) | HotSpot 25 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ProxySplit `prefl` | 30.4-30.9 | 30.4-30.6 | 36.5-37.1 | 36.4-37.8 | 1329-1362 | 41.8-43.7 | 3.1 |
| ProxySplit `prefl`, first rep | 830-905 | 583-599 | 880-988 | 540-936 | 1729-1831 | 603-617 | 53-59 |
| ProxySplit `invoke` | 14.1-14.3 | 14.1-14.3 | 13.6-14.0 | 13.6-14.1 | 14.3-14.5 | 14.2-14.4 | 6.8-7.5 |
| ProxySplit `mhv` | 9.4-10.3 | 9.4-9.7 | 6.1 | 5.8-6.2 | 14.6-16.2 | 13.3-15.8 | 16.7-18.1 |
| MiProbe | 12.2-12.3 | 12.1-12.5 | 12.2-12.4 | 12.2-12.3 | 12.2-12.3 | 12.2-13.3 | 2.9-3.1 |

In a trace of the first optimizing compile of `$Proxy0.price`, the handler site went from
`ir-profile-guard-miss obs=28-29` in 6 of 6 runs (3 G1, 3 ZGC) to a guarded splice in 6 of 6.
`FrozenRowsBench facts`, `AsTypeEdge` and `R21ReflectInvoke` give HotSpot's answers in both modes.

## Update: a handle guessed inside a spliced helper; where the remaining nanoseconds go

A non-final static handle read inside a helper the loop splices (`mhvs`: `viaHelper` reads
`MHV.invokeExact`) now folds behind the same identity guard as one read at the method's own call.
Last 3 reps, ns/op:

| Row | dev (ZGC) | now (ZGC) | dev (G1) | now (G1) | HotSpot 25 |
| --- | --- | --- | --- | --- | --- |
| ProxySplit `mhvs` | 716-722 | 9.6-9.9 | 866-887 | 13.9-16.3 | 15 |

Probe output parity against HotSpot is unchanged from dev in both modes.

A perf profile of the warmed rows (JIT frames named through the perf map) shows that the remaining
gap is not the call to the target method:

| Row (G1) | ns/op | caller's optimizing body | target `lambda$static$0` | allocation zeroing |
| --- | --- | --- | --- | --- |
| ProxySplit `mhc` | 8.5 | 67 % | 12 % | 11 % |
| ProxySplit `invoke` | 14.3 | 80 % | 14 % | - |

- The folded handle's target method is now spliced when its body cannot raise (see below), which
  removes its 12-14 %. 3 interleaved rounds, 2000 reps, last 3, ns/op: `mhc` 8.5-8.7 -> 5.9-6.3 on
  G1, 8.8-8.9 -> 6.2-6.4 on ZGC; `mhv`, `mhvs`, `invoke` and `prefl` unchanged. The rest of HotSpot's
  0.3 ns comes from removing the argument and result boxes after inlining. Here `Integer.valueOf` is
  spliced, not a call, and the convert site's guards keep the box live in their deopt snapshots, so
  the box/unbox fold (`ea_ir_bridge`) cannot apply.
- The caller body of `invoke` (MiProbe has the same shape) has no hot helper call (no
  `jit_instanceof` either: the lambda-proxy way answers the receiver check inline). The samples sit
  on constant-root reloads every iteration (`IndyRootRef` loads and the field loads that depend on
  them, reloaded because a collector may move the object at any safepoint) and stack home stores
  and slot clears.

## Update: the target lambda behind a constant `Method` is spliced

On a lambda receiver the target method (14 % of `invoke`) stayed a call: inside the spliced
`DirectMethodHandleAccessor.invokeImpl`, the accessor's `target.invokeExact(obj, arg)` is
signature-polymorphic, so the inline resolver refused it, and the builder folded the handle to
`Service.price` only during the walk. It is now planned there for the receiver's exact class and
spliced through the fold (see Fixed here). Two gaps hid that class: the root site's varargs array
stopped the operand walk, and the receiver-profile nested routes handed their bodies no operand
classes. dev vs dev with this change, rounds 2-3 of 3 interleaved, 2000 reps, last 3, ns/op:

| Row | dev (G1) | now (G1) | dev (ZGC) | now (ZGC) |
| --- | --- | --- | --- | --- |
| ProxySplit `invoke` | 10.9-11.2 | 9.2-9.6 | 11.3-11.6 | 8.5-9.4 |
| ProxySplit `prefl` | 38.9-41.4 | 23.3-24.8 | 39.9-40.7 | 26.1-26.4 |
| ProxySplit `invacc` | 8.0-8.3 | 5.2-5.6 | 7.8-8.4 | 5.3-5.5 |
| ProxySplit `mhvs` | 5.8-6.0 | 2.6-2.9 | 5.9-6.2 | 2.5-5.2 |

`mhc`, `mhv`, `invcls` and MiProbe (a class receiver) are unchanged. The jit lib suite fails the same
83 tests on both builds; the vm lib suite differs only by `l1_dispatch_tests`, which fails on dev too
in isolated runs. The probe facts match HotSpot as dev's do: the `null-receiver` message rows flip
between runs on both builds (whether the fast-throw profile has the site yet).

## Fixed here

- `lambda_proxy_satisfies` (`Class.isAssignableFrom` / `checkcast` of a lambda proxy) answered a proxy
  against its own functional interface only after taking the class-manager lock and allocating the
  target's name. Every reflective call on a lambda receiver pays it once
  (`DirectMethodHandleAccessor.checkReceiver`). The class-id identity now answers first.
- The call lowering takes a kind-1 row's bound-site inline cache again (see the update above).
  Test: `ic_cascade_kind_tests`.
- `refersTo`'s compare-only referent read under ZGC's referent gate:
  `referent_read_owes_the_checked_helper` lets a read whose every compare is against one value
  through inline, the arm testing that value for null first (`refersTo(null)` is the gate's only
  question) unless it can never be null; the VM names the compare-only row on every collector
  (`reference_referent_row_words`). Test: `j27_refs_a_referent_read_owes_the_helper_when_the_gate_may_arm`.
- The method-handle constant arms asked for a `LoadStatic` of the handle's field, but the
  static-final fold builds that read as a constant root: every `static final` handle site refused the
  method (`IrBuilder::is_static_field_read`). The constant-handle fold dropped interface members; it
  folds them behind an `instanceof` receiver guard (`plant_member_class_guard`).
- `DirectMethodHandleAccessor.checkReceiver`'s `C.isAssignableFrom(o.getClass())` on a constant
  mirror is built as `o instanceof C` (`ConstFoldTables::const_mirror_classes`).
- A folded constant keeps its exact class (`const_ref_exact_classes`), so `ma.invoke(..)` on the
  folded accessor splices with no class test and no miss call.
- Box calls: a `checkcast` and `getfield value` of `Integer.valueOf(x)` are not built; the box fold
  ignores call-site exception flags and runs after the null-compare fold; a lone box named only by
  snapshots folds to an auto-box deopt recipe; a reflective call's varargs box stays a call
  (`reflective_varargs_box_site`) and folds once the array is gone.
- Escape analysis scalar-replaces small reference arrays (a varargs `Object[]`); the deopt
  materialiser rebuilds them by component class; the lowerer describes an object whose deopt sites
  span several blocks when every site agrees (`object_agreeing_site`).
- The interpreter's tier-up offer walked its method's constant folds without its thread, so it could
  not decode a constant handle and minted no handle row. A spliced proxy body's handle then folded only
  if a later recompile found a row another path left in the same collection epoch. The offer now hands
  its mutator to the walk, as the OSR offer did (`offer_invocation_to_tiered_manager`).
- A non-final, non-volatile static holding a `MethodHandle` gets a rooted slot and the same handle
  folds (`ConstFoldTables::static_guess_const_refs`). An `invokeExact` on a read of it is guarded
  `handle == slot` at the call and folded; a failure deopts to the call, and past the trap limit the
  site is de-speculated. Test: `static_guess_mh_tests`.
- The splice resolver gave no row to a call, field access or numeric `ldc` at a pc its prune feed
  called dead, but the builder prunes only the branches it can (it declines without chain state, in a
  sync splice, behind a replay fence) and walks the rest, so a body with such an arm bailed its whole
  build (`Method.invoke`'s caller-sensitive arm, `checkAccess` and `verifyAccess` slow paths,
  `Method.isCallerSensitive`'s first-call arm). Those calls now get plain rows that never refuse,
  nest or bind a callee; the planner leaves them out of its unbindable-call refusal, its leaf test
  and its dispatch cost (`ir_splice_row_is_prune_dead`). This is the `--compatible` `prefl` and FRB
  `proxy` gain above. Unit test: `a_prune_dead_call_row_is_neither_unbindable_nor_a_call`.
- The gate's cold-arm weight (`splice_cold_arm_ranges`) priced a spliced blind dispatch as cold only
  when ONE rare branch's taken edge alone reached it. It now also removes the rare edges together,
  rare fall-through edges included (a `throw` arm after an always-taken check), and prices the code
  only they reach at the sum of their weights. `verifyAccess`'s `slowVerifyAccess` (reached from the
  access-cache miss and from the never-entered protected-member arm) is now cold. Unit tests:
  `cold_arm_union_tests`.
- The invoke doors' receiver and call-site profile coalescing counted a sampled record as one call,
  so a monomorphic site reached the guarded-splice evidence bar only after ~27k calls, and receiver
  shares favoured sites whose receivers alternate. Both coalescing layers now record the calls each
  record stands for (`coalesced_record_weight`, `ReceiverRecorder::record_n`). Tests:
  `coalesced_records_weigh_every_call_they_stand_for`,
  `a_weighted_record_counts_every_call_it_stands_for`. A streak that ended between two sampled
  records dropped the calls it carried; they are now recorded when the streak ends. Test:
  `an_ended_coalescing_streak_flushes_its_carried_calls`.
- A non-final static handle read inside a spliced body is guessed too: the guard resumes through the
  chain snapshot at the call, and a trap is charged to the splice's outermost `invoke`
  (`plant_static_guess_guard`). Tests: `a_guess_read_inside_a_spliced_body_folds_behind_its_identity_guard`,
  `a_spliced_guess_charged_to_its_outer_invoke_keeps_the_handle_call`.
- A box-returning converting handle call (`asType` of a primitive member) kept its member as a call:
  the planner skipped both of the site's calls, since a trap inside a spliced body would re-execute
  the `invokeExact` with its arguments already converted. A member body that cannot raise (and one
  static body it calls) has no trap to resume: it is planned at the site's `getstatic` pc for the
  receiver's exact class, spliced in place of the call, and its value is boxed after it
  (`IrBuilder::convert_member_splice_admitted`, `box_convert_member_result`). Tests:
  `mh_convert_member_splice_tests`.
- A folded constant handle's member inside a spliced body (`Method.invoke` -> `invokeImpl` ->
  `target.invokeExact`) is planned at the refused `invokeExact` for the receiver's exact class
  (`folded_handle_member_row` in the VM resolver), spliced only through the fold when its body
  cannot raise and the receiver is proven (`IrBuilder::folded_member_splice_admitted`), and never
  by the invoke arms (`signature_polymorphic_site`). Tests: `mh_folded_member_splice_tests`.
- A nested call's operand walk stepped over no inline varargs array, so `M.invoke(LAMBDA, i)` handed
  `Method.invoke` no operand classes; it now steps over `anewarray` / `newarray`, `dup` and the
  array stores (`splice_call_operand_sources_with_calls`). Tests: `splice_operand_varargs_tests`.
- The receiver-profile nested routes resolved their bodies with no operand classes; they now hand
  the call's operand classes down, as the static routes do.

## Next steps

The remaining work is scoped in
[`../perf/reflective-and-method-handle-calls-remaining-gaps.md`](../perf/reflective-and-method-handle-calls-remaining-gaps.md).
