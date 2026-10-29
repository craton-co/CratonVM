# A constant real bound handle and `bindTo` in a loop walked the link chain (MH6-3, MHB-3)

> **j27 wave 8 (lane W8DSP, 2026-10-28): REVIEWED (no change), PENDING HOST VERIFICATION.** Re-read wave 4's
> static bound admission (`native-builtins/src/lang_invoke/adapters.rs` `mh_real_bound_compiled_bind`,
> `mh_real_bound_member_kind`, `mh_real_bound_leading_value_ok`): the site must equal the bound handle's
> type, the direct handle must be at its own type (an `asType`-stamped one refuses), the leading parameter must
> be an `L..;` class, and the bound value a non-array instance of the ONE loaded class of that name (two loaders
> defining it, or an interface `is_subclass` does not answer, refuse: sound, the chain is kept). No defect.
> Still open, not built here: the compile-time bound STATIC row (MH16-7's compile half) -- it needs the
> `jit_bridge` kind-4 door, the `lib.rs` planner and `ir.rs` `mh_constant_bound_opcode` to accept a static
> member whose first argument is the `argL1` load, and its helper half is not host-verified yet; building it
> blind would stack an unmeasured change on an unmeasured one. Order for the host: wave 4's `J27MhRows`
> recipe first (facts and the `bound-static-*` timing rows), then the compile half.

> **j27 wave 5 (lane MHASTYPE, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** No change to
> the bound handles of this page.
>
> The compile-time bound STATIC row (MH16-7's compile half, item 1 of
> `j27-mh-real-astype-converting-bind-proposal-20261028.md`) is still open. It needs the
> jit_bridge door, the `jit/src/lib.rs` planner and `jit/src/ir.rs`, which are outside this
> lane's files, and MH16-7's helper half is not host-verified.
>
> Done in this area: the real `asType` converting bind, behind the opt-in
> `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`. Its record is the proposal page above.
>
> Review of `mh_constant_bind_fill`: a converting real handle is now kept out of the
> `mh_constant_records_kind` compile rows (`real_conv.is_none()`), as the carrier's converting
> one already was (`conv.is_none()`). The bound compile row was already safe: it requires
> `t.bound`.
>
> `mh_real_compiled_bind`'s kind and lane-record halves moved unchanged into
> `mh_real_direct_kind` / `mh_real_lane_record`, which the new admission shares.
>
> Verify: every `J27MhRows` fact must stay HotSpot's with the switch unset; the
> `bound-static-*` lines must be unchanged.

> **j27 wave 4 (lane MHROWS, 2026-10-28): PARTLY (MH16-7 added), PENDING HOST VERIFICATION.** The
> real-bound admission covered virtual members only; a `findStatic(..).bindTo(x)` (or
> `insertArguments(.., 0, x)`: the same `Species_LL` and `BIND_ARG(1)` form) walked the four-hop link
> chain. `lang_invoke::mh_real_bound_compiled_bind` now also binds a `REF_invokeStatic` direct handle
> on its leading reference parameter (`invoke_kind 3`, `bound`; pure type questions in
> `mh_real_bound_member_kind`, value check `mh_real_bound_leading_value_ok`); the helper's bound route
> splices `argL1` as the static leaf's first argument. Switch `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC`
> (perf, default ON, `=0` off). Helper half only (no compile-time row). Probe
> `tools/probes/jit/j27/J27MhRows.java`, `--jdk-only` and `--compatible`, default and `=0`; HotSpot 25
> prints exactly (timing lines only with `-Dj27.time=true`):
>
> ```
> fact exact-static-sum 9999900000 same=true
> fact unbound-virtual-sum 5000250000 same=true
> fact bound-virtual-sum 5000450000 same=true
> fact bound-static-long-sum 5003950000 same=true
> fact bound-static-short-sum 5000250000 same=true
> fact bound-static-char-sum 5006450000 same=true
> fact bound-static-boolean-sum 4999950000 same=true
> fact bound-static-null-len 988890 same=true
> fact bound-static-string-len 688890 same=true
> fact bound-static-pair-sum 5004000000 same=true
> fact bind-each-static-sum 200060000 same=true
> fact conv-mixed-sum 12696928 same=true
> fact bound-static-long 42 java.lang.Long
> fact bound-static-short 5 java.lang.Integer
> fact bound-static-char 66 java.lang.Integer
> fact bound-static-true 7 java.lang.Integer
> fact bound-static-false -7 java.lang.Integer
> fact bound-static-null null:5 java.lang.String
> fact bound-static-string s:5 java.lang.String
> fact bound-static-type (int)long java.lang.String
> fact bound-static-wmte java.lang.invoke.WrongMethodTypeException: handle's method type (int)long but found (int)int
> fact bound-static-throw java.lang.IllegalStateException: boom 3 top=boom java.lang.String
> fact bind-to-long-with-integer java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Long
> fact bind-to-long-with-short java.lang.ClassCastException: Cannot cast java.lang.Short to java.lang.Long
> fact bind-to-char-with-integer java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Character
> fact bind-to-boolean-with-string java.lang.ClassCastException: Cannot cast java.lang.String to java.lang.Boolean
> fact bind-to-primitive-leading java.lang.IllegalArgumentException: no leading reference parameter
> fact bound-virtual-null-receiver java.lang.NullPointerException
> fact conv-long-Long 10 java.lang.Long
> fact conv-long-Integer 14 java.lang.Long
> fact conv-long-Short 6 java.lang.Long
> fact conv-long-Byte -4 java.lang.Long
> fact conv-long-Character 130 java.lang.Long
> fact conv-long-Boolean java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Long
> fact conv-long-null java.lang.NullPointerException
> fact conv-short-Short -4 java.lang.Short
> fact conv-short-Byte -5 java.lang.Short
> fact conv-short-Integer java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Short
> fact conv-short-Character java.lang.ClassCastException: Cannot cast java.lang.Character to java.lang.Short
> fact conv-short-Boolean java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Short
> fact conv-short-null java.lang.NullPointerException
> fact conv-char-Character B java.lang.Character
> fact conv-char-Integer java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Character
> fact conv-char-Short java.lang.ClassCastException: Cannot cast java.lang.Short to java.lang.Character
> fact conv-char-null java.lang.NullPointerException
> fact conv-boolean-Boolean false java.lang.Boolean
> fact conv-boolean-Integer java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.Boolean
> fact conv-boolean-null java.lang.NullPointerException
> fact conv-invoke-Short 18 java.lang.Long
> fact conv-invoke-Boolean java.lang.ClassCastException: Cannot cast java.lang.Boolean to java.lang.Long
> fact conv-wmte java.lang.invoke.WrongMethodTypeException: handle's method type (Object)Object but found (Object)Long
> fact exact-wmte java.lang.invoke.WrongMethodTypeException: handle's method type (int)int but found (int)long
> ```
>
> The sums and messages above are derived by reading JDK 25 (`MethodHandle.bindTo`,
> `ValueConversions.unbox*` / `primitiveConversion`, `Wrapper.isConvertibleFrom`,
> `Invokers.newWrongMethodTypeException`); run the probe on HotSpot 25 first and treat ITS output as the
> reference if any line differs. Then `CRATONVM_DBG_JIT_MH_BIND=1`: a bound fill line for the
> `boundStatic*` kernels except `boundStaticNull` (null `argL1` keeps the chain), one fill then shape hits
> for `boundStaticPair` / `bindEachStatic`. Timing (`-Dj27.time=true`, interleaved, medians): the
> `bound-static-*` rows must drop with the switch on, every `fact` unchanged.

> **j27 wave 2 (lane INVOKE, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** (Partial host evidence,
> checkpoint commit `543e6f12ba`, snapshot `bb68e37943`: `J27InvokeBindCount` SAME as HotSpot; the
> `T1W13MhbindRows` timing A/B is not recorded yet.) Two per-call costs of
> `bind-each` / `bind-only` found by reading and removed, both perf, default ON, `=0` kill:
> (1) `CRATONVM_JIT_MH_BIND_COUNT_RAW` -- every `bind-each` call is a shape hit on a FRESH handle, so
> `MhBindHit::customized_seen` is false and `helpers::mh_bind_customization_serves` built a
> `NativeContextImpl` and ran `lane_check_customized` (four descriptor-aware context field doors, about
> 240 ns per two reads as measured in r22) on EVERY call, just to raise a fresh handle's
> `customizationCount` from 0 to 1. Now `dispatch.rs` `mh_bind_handle_count_raw` does that `Count` arm
> from raw slots (`lang_invoke::lane_raw_count_slots`, answered only after this thread's counting path
> settled the slots and a default `CUSTOMIZE_THRESHOLD`; `lane_raw_count_step`); due, customized-later
> and unsettled cases still take the context path. (2) `CRATONVM_MH_BINDTO_REAL_MINT_BATCH`: the
> `bindTo` memo-hit mint (`adapters.rs` `mh_real_bind_to_mint_slots` -> new `real_bind_mint_batched`)
> reads the prototype in one `get_fields_typed` and stores the fresh species in one `set_fields_typed`
> (was 2 reads + 4 stores through single-field doors for `Species_LL`; `RealBindSlots::desc` holds each
> argument's species letter). Not addressed: the native funnel of the compiled `bindTo` call itself,
> and the fused `bindTo` + `invokeExact` (MHBIND21-2). Verify: `tools/probes/jit/j27/J27InvokeBindCount.java`
> (`--add-opens java.base/java.lang.invoke=ALL-UNNAMED`), both modes, default and each switch `=0`;
> HotSpot 25 prints `fact bind-each-sum 20001400000`, `fact fresh-called-once-count-1 196 other=none`,
> `fact after-100 100 customized=false`, `fact after-127 127 customized=false`,
> `fact after-128 127 customized=true`, `fact after-1128 127 customized=true sum=512698`,
> `fact bind-only-sum 200000`. Then `tools/probes/t1/T1W13MhbindRows.java` default vs each `=0`
> interleaved (medians): `bind-each` / `bind-each-mixed` must drop with COUNT_RAW on, `bind-only`
> (and `bind-each`) with MINT_BATCH on; every `fact` line unchanged. The existing customization probes
> (`R22InlineMhCustomize`, `R22W4RuntimeEdges mh-fresh-thresholds`) must stay HotSpot's.

> **j27 wave 1 (lane MHINDY, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Found by reading a
> likely reason `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND=0` did not move `const-bound` (898 vs 836 ns
> below): `CpResolvers::mh_constant_bound` (and `mh_constant_filter`) looked the fill's address row
> up under `mem.heap.collection_count()` while the fills stamp it with `mem.address_epoch()`; once
> the two diverge (G1 region recycle, any concurrent sweep slice) every kind-4 site lost its row and
> the jit crate dropped it. Fixed in `vm/src/runtime/interpreter/jit_bridge.rs` (both doors read
> `address_epoch()`), page `j27-mh-constant-bound-and-filter-doors-read-the-wrong-epoch-20261028.md`.
> No switch. Verify: `tools/probes/t1/T1W13MhbindRows.java`, default vs `..._COMPILE_BOUND=0`
> interleaved, G1 and the default collector, both modes; `CRATONVM_DBG_JITC=1` must print
> `[ir] mh CONSTANT BOUND` for `constBound` / `boundVirtual` / `boundSelf`, and `const-bound` must
> drop toward tens of ns. If it does not, read `CRATONVM_DBG_JIT_MH_BIND=1`'s `[mh-bind] compile`
> line (the remaining suspects: the method compiled before the fill and no MH7-1 recompile ran, or
> the planner's splice of the member). `bind-each` / `bind-only` (the `bindTo` allocation per call,
> MHBIND21-2) are unchanged: not addressed this wave.

> **VERIFIED AGAINST A BINARY 2026-10-27 (t1 orchestrator): correctness holds; the page stays open as
> perf.** Host 64.177.43.150, cvm-d5 (dev `51ccac482f`), the rebuilt probe `tools/probes/t1/T1W13MhbindRows.java`
> (the original `R21MhbindRows` source was lost with `C:\craton\jr21-ssh`): every `fact` line (the six sums,
> `wmte-hot`, `boom-hot ... top=boom`, `cce`, `iae`, `npe`, `insert-args`, `filtered-sum`, `bind-array-for-string`,
> `bind-string-for-array`) SAME as HotSpot 25 in all 16 runs (default + 7 switch-off arms x both modes). One rep
> each, ns/op, `--jdk-only` (`--compatible` agrees):
>
> | arm | const-bound | bound-virtual | bound-self | bind-each | bind-each-mixed | bind-only |
> |---|---|---|---|---|---|---|
> | HotSpot 25 | 2 | 0 | 0 | 79 | 90 | 104 |
> | default | 898 | 894 | 1004 | 5837 | 5480 | 1563 |
> | `..._REAL_BOUND=0` | 21156 | 14154 | 8519 | 14587 | 18572 | 1156 |
> | `..._REAL_SHAPE=0` | 999 | 957 | 955 | 24203 | 22054 | 1148 |
> | `..._REAL_SHAPE_TWO_CLASSES=0` | 916 | 900 | 895 | 4918 | 22318 | 1463 |
> | `MH_BINDTO_REAL_MEMO_TWO_CLASSES=0` | 564 | 540 | 559 | 7082 | 11723 | 1681 |
> | `..._COMPILE_BOUND=0` | 836 | 565 | 692 | 3150 | 3217 | 923 |
>
> So REAL_BOUND, REAL_SHAPE and the two TWO_CLASSES switches earn their default ON. COMPILE_BOUND does not move
> `const-bound` (expected tens of ns): read its `[mh-bind] compile` line next. The gap to HotSpot (~450x on the
> constant bound handle) is the open perf item.

Status: PENDING HOST MEASUREMENT (JIT round 21 wave 19, lane mhbind2: MHBIND21-1 -- the constant real bound handle compiled as its member's call -- and the bind-each-mixed fixes are in code behind four new default-ON switches, section "Round 21 wave 19"; wave 17's three switches (lane mhbind) are in and measured: REAL_BOUND and REAL_SHAPE win, see the table there. `filtered` is filed separately)

Re-land (20261026): merge `624fae0b49` had dropped this round-21 code from dev; it is back on dev since `44f0382076` (formatted 3-way re-land, `docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md`). The status above describes the code again; its host verification is recorded there.

Area: JIT dispatch helpers (`vm/src/jit/helpers.rs` constant bind) and `native-builtins/src/lang_invoke.rs` (compiled-bind admission, real `bindTo` memo)
Found by: JIT round 21 lane mhbind (verifying the two `(unverified)` high rows of `jr21-r18-backlog-retriage-20261015.md` section 2.1)
Probe: `C:\craton\jr21-ssh\probes\R21MhbindRows.java`

## Verification of the rows (both were really missing on 2a0ec6cbb6)

Since the `JdkInvoke` alignment row (both modes, `types/src/hotspot_alignment.rs` row 78) `findVirtual`
answers a REAL `DirectMethodHandle` and `bindTo` (a reviewed `Intrinsic`) runs the JDK's own body
(`mh_foreign_bind_to`) or the SBR5 per-receiver memo mint (`mh_real_bind_to`). The JDK's answer for
`dmh.bindTo(x)` is `BoundMethodHandle$Species_LL(type, F, argL0 = dmh, argL1 = x)`, where `F` is the
`BIND_ARG` position-1 transform of the `LF_REBIND` reinvoker form (`MethodHandle.bindArgumentL` ->
`DirectMethodHandle.rebind` -> `BoundMethodHandle.makeReinvoker`, then `LambdaFormEditor.bindArgumentL`).

* **MH6-3** (constant bound handle). `lang_invoke::mh_compiled_bind_target_with` sent every real handle to
  `mh_real_compiled_bind`, which binds only a `DirectMethodHandle` (`record.direct`). A real species was refused,
  so a compiled `CB.invokeExact(i)` on a `static final` bound handle took `mh_compiled_link_redirect`'s chain:
  linker (`Invokers$Holder.invokeExact_MT`), `invokeBasic` on the species (its spun `LambdaForm$MH`), `invokeBasic`
  on the direct handle (`DirectMethodHandle$Holder.invokeVirtual`), `linkToVirtual`: four helper round trips per
  call. The carrier-only bound arm (`CRATONVM_JIT_MH_CONSTANT_BIND_BOUND`, `MH_BOUND`) never sees a real handle. The
  compile-time path (`jit-mh-constant-compile`, `ir_mh_constant_plan`) records only unbound handles.
* **MHB-3** (`bindTo` in a loop). Per call: the compiled `bindTo` site reaches the native intrinsic; on a memo hit
  `mh_real_bind_to_mint` resolved every species field BY NAME and built `"arg{ch}{i}"` with `format!` per bound
  argument (two `String`s and six by-name resolutions for `Species_LL`). The fresh handle then reached the
  constant-bind redirect at a new address every call: a refill per call (refused, as above) until
  `MH_BIND_MAX_FILLS`, then the four-hop chain for good.

## Round 21 change

1. **MH6-3, `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND`** (perf, default ON, `=0` off). New admission arm
   `lang_invoke::mh_real_bound_compiled_bind`: a real `Species_LL` whose `argL0` is a plain
   `java/lang/invoke/DirectMethodHandle` of `REF_invokeVirtual` at its member's own exact type, whose `argL1` is
   non-null, whose `type` is the site's descriptor and equals the member's own descriptor, and whose `form`
   (uncustomized) IS the cached `BIND_ARG(1)` transform of `dmh.type.form.basicType.form.lambdaForms[LF_REBIND]`
   (`mh_real_is_bind_to_form`: `packedBytes == 0x11`, referent identity; a `ConcurrentHashMap` cache or a cleared
   soft reference refuses). Then the carrier's member rules (`mh_compiled_bind_member`, the bound value a non-array
   instance of the member's class). `MhCompiledBind::bound`; the dispatch helper's fill
   (`helpers.rs` `mh_constant_bind_fill`) writes `argL1` (`mh_real_bound_receiver`) into the guard word the
   redirect reads a bound receiver from. The warm guard is the handle's address in the epoch (every field `final`,
   `MhBindHit::real`), the call `mh_constant_bind_call` -> `mh_bind_direct_call` with the receiver spliced.
   Why it is right: `F` is `invokeBasic(argL0, argL1, a*)`; `invokeBasic` of a plain virtual direct handle is
   `linkToVirtual(argL1, a*, member)` with no receiver check; the admission asks the same type and receiver
   questions the carrier's bound arm asks. Exceptions, deopt and GC are the existing bound route's.
   The form check is load-bearing: a `filterArguments(dmh, 0, f)` species is also `Species_LL` over the direct handle.
2. **MHB-3 (a), `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE`** (perf, default ON, `=0` off). A site bound to a real
   `bindTo` result records `MhRealBoundShape` (species class, slots of `argL0`/`argL1`/`form`/`type`, the admitted
   handle's `argL0`, `form`, `type` words and the bound value's exact class). A handle at another address of that
   shape (`mh_real_bound_shape_receiver`) is called with ITS `argL1` as receiver, no refill. Sound: the same
   direct-handle and form objects (equal address, equal epoch) and the same `type` object answer the admission
   identically; a bound value of the admitted value's exact class passes its receiver question; a customized form
   belongs to one instance, so another instance never matches it.
3. **MHB-3 (b), `CRATONVM_MH_BINDTO_REAL_MINT_SLOTS`** (perf, default ON, `=0` off). `RealBindLine` keeps the
   species' resolved slots (`RealBindSlots`, filed with the line, read only when filing); a memo hit mints with
   `mh_real_bind_to_mint_slots` (same instance, same stores in the same order, by slot, no `format!`).

## Review finding, fixed (correctness, unconditional)

`mh_real_bind_to`'s memo (SBR5 lane avh) admitted a repeat `bindTo` when the bound value's
`class_id_of_object` equalled the line's, but for a reference ARRAY that call answers the COMPONENT class
(`native-api` `object_is_array` doc). So after `lenHandle.bindTo("abc")` (a `String`-leading handle),
`lenHandle.bindTo(new String[]{..})` hit the line and minted a species with the array as `argL1`, skipping the JDK
body's `leadingReferenceParameter().cast(x)`: no `ClassCastException` (HotSpot: `Cannot cast
[Ljava.lang.String; to java.lang.String`), and a later `invokeExact` calls `String.length` on an array. The reverse
(a `String[]` line admitting a `String`) too. Now an array value never hits and a line filed for one keeps no
class. Probe facts `bind-array-for-string-{0,1,2}`, `bind-string-for-array-{0,1,2}` (all three must be HotSpot's
CCE line, both modes; by the reading above every row printed `no-throw` before the fix: a positive control is
the parent commit's binary).

Not built (proposals `jit-r21-mhbind-proposals.md`): the guard inline in compiled code / at compile time
(MHBIND21-1), a fused `bindTo` + `invokeExact` without the species allocation (MHBIND21-2), other species shapes
(MHBIND21-3, MHBIND21-4).

## How to verify

`R21MhbindRows`, `--jdk-only` and `--compatible`, against HotSpot 25:

* every `fact` line equal to HotSpot's in every arm below (sums of every row, `wmte-hot`, `boom-hot` with
  `top=boom`, `cce`, `iae`, `npe`, `insert-args`, `filtered-sum`, `bind-array-for-string-*`,
  `bind-string-for-array-*`);
* `CRATONVM_DBG_JIT_MH_BIND=1`: a bound fill line for `constBound`, `boundVirtual`, `boundSelf`; for `bindEach`
  ONE fill (then shape hits), for `bindEachMixed` refills until the site gives up; `filtered` never bound;
* `CRATONVM_DBG_MH_LINK_CENSUS=1`: the `const-bound` / `bind-each` link rows drop to ~0 per call in the default arm.

Timing arms (interleaved, medians): default vs `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND=0` decides
`const-bound`, `bound-virtual`, `bound-self` (expected several-fold: four helper hops to one); default vs
`CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE=0` decides `bind-each`; default vs `CRATONVM_MH_BINDTO_REAL_MINT_SLOTS=0`
decides `bind-only` (and `bind-each`). A row that does not move with its switch: read the fill lines first (a refused
`bound` fill means a form / type shape this page did not foresee).

Risk to watch: the `BIND_ARG` scan reads JDK-private field names (`LambdaForm.customized/transformCache`,
`MethodType.form`, `MethodTypeForm.basicType/lambdaForms`, `LambdaFormEditor$Transform.packedBytes`,
`Reference.referent`). A JDK whose layout differs makes the check answer `false` (the chain, as before), never a
wrong bind; a zero-bind result on JDK 25 means one of those reads does not resolve here.

## Round 21 wave 19 (lane mhbind2): MHBIND21-1 and bind-each-mixed

Host numbers this wave started from (round head 5906cd86a4, `--jdk-only`, median ns/op):

| row | default | HotSpot | REAL_BOUND=0 | REAL_SHAPE=0 |
|---|---|---|---|---|
| bind-each | 1546 | 73 | 4280 | 3937 |
| bind-each-mixed | 8874 | 41 | 7010 | 6968 |
| bind-only | 1124 | 40 | | |
| bound-self | 324 | 1 | 5274 | |
| bound-virtual | 358 | 3 | | |
| const-bound | 364 | 2 | | |
| filtered | 5830 | 2 | flat | flat |

`CRATONVM_MH_BINDTO_REAL_MINT_SLOTS=0`: bind-each 1868, bound-virtual 230.

Wave 17's MH6-3 took const-bound / bound-self / bound-virtual from ~5 us to ~350 ns: the dispatch
helper's bound route (`jit_invoke_dispatch` -> the per-thread `MH_BIND_SITES` probe -> `mh_bind_direct_call`).
HotSpot's 1-3 ns is the member inlined at the call. That is MHBIND21-1.

### 1. MHBIND21-1: the constant real bound handle at compile time (`CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`, perf, default ON, `=0` off)

* **Record** (`vm/src/jit/helpers.rs` `mh_constant_bind_fill`, new `mh_real_bound_compile_row`): when the fill
  binds a REAL `bindTo` result of a virtual member (`target.bound`, `real`, `invoke_kind 0`, the
  `argL1` word already read as `real_receiver`), it records the handle for the compile doors as
  `invoke_kind 4` with `(argL1 slot, bound value's exact class id)`
  (`jit_bridge::note_mh_constant_target_bound`; `MhConstantTarget::bound`). The slot must lie inside the
  instance and hold that very word. Same memo, epoch and waiter rules as every other row
  (`note_mh_constant_target_row`, the old body of `note_mh_constant_target_kind`, moved unchanged), so a
  method whose optimizing compile found no admission yet is recompiled once the fill admits it (MH7-1).
* **Door** (`jit_bridge.rs` `CpResolvers::mh_constant`, arm `4`): an instance, non-private,
  non-abstract member of a class whose descriptor IS the site's (the receiver is no site argument), the
  site's classes one class from both loaders (`mh_constant_descriptor_agrees`), and the existing member
  rules (resolves from the caller to the recorded class id, public / own class / nonpublic rule,
  not synchronized / native). New resolver `CpResolvers::mh_constant_bound` ->
  `CompileRequest::cp_mh_constant_bound_resolver`: the `(slot, class)` of the same row (field row first,
  then the address row of this epoch); `None` drops the site.
* **Plan** (`jit/src/lib.rs` `ir_mh_constant_plan`): kind `4` is planned under the virtual-call gate when
  the member's descriptor equals the site's and the site's receiver guard never trapped; it joins the
  invoke plan as an `invokevirtual` row of the member keyed by the field (`rewritten_invoke_ops`, as
  kind 0). `IrMhConstantPlan::with_bound_rows` then asks the bound resolver per field and drops a kind-4
  field with no row. The planner's guarded-splice class at such a pc is the bound value's exact class
  (`bound_receiver_class_at`, ahead of the receiver profile, which at that pc records the HANDLE's class):
  the member body selected for that class is spliced behind one exact class test, the plain virtual call
  on the miss edge.
* **Build** (`jit/src/ir.rs` `IrBuilder::mh_constant_bound_opcode`, from `mh_constant_opcode`): the handle
  on the abstract stack (where the receiver goes; the row counts the receiver) must be the planned
  field's `Op::LoadStatic`; it is REPLACED by `Op::Load(Ref)` of its `argL1` slot (memory chain advanced as
  `getfield`; pc listed in `synthetic_field_access_pcs`, so no per-pc compact row describes it), null-guarded
  at `pc` with a deopt that re-executes the `invokeExact` (`indy_direct_receiver_guard`), pc pushed on
  `mh_constant_call_pcs` (no post-call exit), and counted as a builder expansion (acceptance evidence).
  The `0xb6` arm builds the call.

Why it is right: the species' form is `invokeBasic(argL0, argL1, a*)` and `invokeBasic` of a plain
`REF_invokeVirtual` direct handle is `linkToVirtual(argL1, a*, member)`: the member SELECTED from the bound
value's class, which is what `invokevirtual member` on `argL1` does. Wave 17's admission
(`mh_real_bound_compiled_bind`) already proved the handle's type is the site's, the form is the `bindTo`
one, and the bound value is a non-null non-array instance of the member's class. Every field read is
`final`, the field is a `static final` of an initialized class (the door's existing checks), and an equal
address in an equal epoch is the admitted object, so the load answers that same value forever. A
`WrongMethodTypeException` site (`(I)J` on CB) has no row (rows are keyed by the site descriptor). A
throwing member keeps HotSpot's trace (the LambdaForm frames are hidden there too).

### 2. bind-each-mixed: why it is slow in every arm, and why the default lost to the off arms

* **Every arm: the `bindTo` memo thrashes.** `mh_real_bind_to`'s memo line keeps ONE bound-value class
  (`x_class`). `MIXED` alternates `Acc` / `Acc2`, so every `PLUS.bindTo(MIXED[i & 3])` missed the line,
  ran the JDK body (8-11 us interpreted, LambdaFormEditor lookup + species factory) and re-filed the
  line for the other class. That is the ~3 us gap between bind-each-mixed (~7000 ns, off arms) and
  bind-each (~4000 ns, off arms). Fix: `CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES` (perf, default ON,
  `=0` off; `native-builtins/src/lang_invoke.rs` `RealBindLine::x_class_alt`): when the JDK body answers
  for the same receiver (same VM, epoch and address as the line it replaces), the replaced line's class is
  kept as the alternate. Sound: both classes passed THIS receiver's `leadingReferenceParameter().cast(x)`,
  a function of the receiver's `final` `type` and the value's class; the species, form and type of the
  answer do not depend on the value. Arrays never hit (wave 17's correctness rule kept).
* **Default arm: the site's shape thrashes too.** `MhRealBoundShape` keeps one bound-value class, so
  with REAL_SHAPE on every other call missed the shape and re-filled (the full real-bound admission: two
  handle records and the `BIND_ARG` form scan) until `MH_BIND_MAX_FILLS`, and the site then walked the
  chain. Fix: `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES` (perf, default ON, `=0` off;
  `helpers.rs` `MhRealBoundShape::receiver_class_alt`): a fill whose new shape equals the site's previous
  one (same VM, same epoch) in everything but the bound value's class keeps the previous class as the
  alternate. Sound: the admission's only question about the bound value is "non-array instance of the
  member's class", which a value of either admitted exact class answers as the admitted one did.
* **Default arm: the interpreter's fill paid the real-bound admission per fresh handle.** Since wave 17
  `mh_compiled_bind_target` runs the real-bound admission (two `real_handle_record`s, the form scan) on
  every REAL species; `jit_bridge::mh_constant_interpreter_fill` (keyed by handle ADDRESS, so a fresh
  `bindTo` result always misses its memo) asked it and then dropped the bound answer (`!t.bound`). With
  REAL_BOUND=0 the same call refuses after one cheap record. Fix: `CRATONVM_JIT_MH_INTERP_FILL_SKIP_BOUND`
  (perf, default ON, `=0` off): the interpreter fill asks `lang_invoke::mh_compiled_bind_target_unbound`
  (the admission with its `bindTo` arms off: identical answers for everything it records).
* What was NOT established by reading: how much of the 1.9 us default-vs-off gap each of the last two
  items is. The give-up after `MH_BIND_MAX_FILLS` bounds the shape thrash to a few fills per compiled
  body, so if `CRATONVM_DBG_JIT_MH_BIND=1` shows only a handful of `bindEachMixed` fill lines in the
  default arm, the gap is the interpreter fill (interpreted / deopted iterations) or host noise
  (8-40 % floor on this host). The arms below separate them.

### 3. filtered: not moved by any arm (filed)

`FILTERED = filterArguments(PLUS, 0, swap)` is also a `Species_LL` over the direct handle, with the filter
as `argL1` and the `FILTER_ARG` transform as its form. Wave 17's form check refuses it (as it must: the
bind would call `plus` on the FILTER as the receiver). Nothing else binds a real species, so every call
walks the link chain (~5.8 us). Page `jr21-mhbind-filtered-species-walks-the-link-chain-FIXED-20261027.md`,
proposal MHBIND2-1 in `jit-r21-mhbind2-proposals.md`.

### How to verify (wave 19)

Probe `C:\craton\jr21-ssh\probes\R21MhbindRows.java`, `--jdk-only` and `--compatible`:

* every `fact` line equal to HotSpot 25's in EVERY arm below (unchanged list: sums, `wmte-hot`,
  `boom-hot` with `top=boom`, `cce`, `iae`, `npe`, `insert-args`, `filtered-sum`,
  `bind-array-for-string-*`, `bind-string-for-array-*`). `bound-virtual-sum` is the row that would catch
  a non-virtual call of `Acc.plus` on an `Acc2` (it must stay HotSpot's);
* `CRATONVM_DBG_JIT_MH_BIND=1`: a `compile row ... bound (argL1 slot N, receiver class id M)` line for CB,
  CB2, CBS, CBOOM; then the door's `[mh-bind] compile constBound(I)J bci=.. MethodHandle.invokeExact(I)I ...`
  line naming `plus(I)I` as admitted (not `no admitted handle`);
* `CRATONVM_DBG_JITC=1`: `[ir] mh CONSTANT BOUND at bytecode pc .. in R21MhbindRows.constBound(I)J`
  (and `boundVirtual`, `boundSelf`, `boomSite`), and an `ir-profile-guard R21MhbindRows$Acc.plus(I)I ...
  receiver class id <Acc2's>` line for `boundVirtual`.

Timing arms, interleaved medians, each against default:

| arm | decides | expected |
|---|---|---|
| `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND=0` | const-bound, bound-self, bound-virtual | default 350 -> tens of ns or less (the member spliced) |
| `CRATONVM_MH_BINDTO_REAL_MEMO_TWO_CLASSES=0` | bind-each-mixed | default well under the off arm (no JDK body per call) |
| `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES=0` | bind-each-mixed | default near bind-each (shape hits for both classes) |
| `CRATONVM_JIT_MH_INTERP_FILL_SKIP_BOUND=0` | bind-each-mixed, bind-each (interpreted share) | default <= off |

Decides: each switch stays ON when its row improves beyond the host floor with every fact equal; a
`COMPILE_BOUND` row that does not move: read the `[mh-bind] compile` line (`no admitted handle` means the
method compiled before the fill and the MH7-1 recompile did not run: check `bg_compile` / `c2_supersede`).
