# Proxy-define row: the residual is ClassFile-API code quality and a flat VM-side cost

> **j27 wave 8 (lane W8DSP, 2026-10-28): REVIEWED (no change), PENDING HOST VERIFICATION.** Re-read wave 5's VM
> half, `vm/src/vm/realms/cha_class_set.rs`, against JVMS 5.4.6: a listed class is concrete, implements the
> site interface, and reaches the declaring class `D` up its superclass chain with NO declaration of the
> method on the way (stricter than JVMS, which skips private / static ones: a coverage loss, never a wrong
> selection); `D`'s declaration is public, instance and concrete, and `D` is a class, so an interface default
> is never the answer where a class method would win; lambda-proxy and array ids are refused; stubs anywhere on
> the chain refuse. No class-hierarchy assumption is recorded, so a class defined later simply takes the
> interface call. No defect. Wave 5's four-step verify recipe stands (the switch stays opt-in until its A/B).

> **j27 wave 5 (lane PROXY, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Built the page's
> "getters' inlining" step for `invokeinterface`, the one the r24 lane described and left unbuilt:
> the exact CLASS-SET guarded splice, opt-in as `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE=1`
> (risky without a build, and the `invokevirtual` twin measured neutral here). `BufWriterImpl.writeIndex`'s
> `PoolEntry.index()` reaches seventeen entry classes that inherit one `AbstractPoolEntry` body, so the
> site is megamorphic and each call takes `jit_invoke_virtual_mic_body`. Now the VM lists the loaded classes
> whose exact objects an interface call selects that body for (`ClassRealm::cha_interface_class_set`,
> `vm/src/vm/realms/cha_class_set.rs`: implements the interface, superclass chain reaches the declaring class
> with no other declaration, public instance body, no stubs). The profile-receiver resolver hands that list to
> the planner on the site it already returns (`InlineSite::cha_class_set`, filled in `jit_bridge/profile_class_set.rs`;
> no thread-local, so the jit statics ratchet stays at 865). The builder splices the body once, behind `word == c0 | word == c1 | ...`,
> with the interface call as the miss edge (`jit/src/ir/cha_class_set.rs`; `IrBuilder::begin_guarded_receiver_splice`).
> Every member is an exact class, so no watermark or invalidation is involved. A class defined later is not in
> the list and takes the call. The planner seeds such a site with its top profile class when every other
> reader declines (`ir::cha_class_set_seed_class`). The flat VM-door cost (item 2) and the verifier (item 3) are
> unchanged. Unit tests: jit `ir::cha_class_set_tests::*` (5); vm (synthetic-jdk)
> `vm::tests::suite_classloading_jtreg::cha_interface_class_set_lists_exact_selectors_of_the_inherited_body`.
> Probe `tools/probes/jit/j27/J27ProxyRows.java` (proxy creation, handler calls, default methods,
> `UndeclaredThrowableException` wrapping, 600 fresh-loader defines, the index shape). Its header lists the
> expected HotSpot 25 facts.
> **Verify:** (1) `J27ProxyRows` facts must equal HotSpot's in both modes, with the switch off and with `=1`.
> (2) Engagement: `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE=1 CRATONVM_DBG_JITC=1` must print
> `ir-cha-class-set J27ProxyRows$Entry.index()I ... class ids [..6 ids..]`. (3) A/B the row: `Zs9Nd8ProxyCost room|generate 3000`,
> interleaved, switch off vs `=1`, ZGC `-Xmx256m`, main-thread CPU. On the `room` arm, also grep `ir-cha-class-set`
> for `writeIndex`. (4) Flip to default ON only if the A/B wins and the redefinition and hidden-class batteries pass with it on.

> **j27 wave 1 (lane MHINDY, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION (measurement).** No code
> change this wave: the residual is spread (flat profile, no symbol above ~0.65 %), and every next
> step listed under "What would close it" needs the frame-pointer `room` profile first; none is in
> this lane's area (MethodHandle / indy / reflection). Re-measure `Zs9Nd8ProxyCost room|define|generate
> 3000` on the merged j27 tree (ZGC, `-Xmx256m`, main-thread CPU, interleaved) so the next lane starts
> from current numbers.

Status (2026-10-24, JIT r24 `kpc` lane; updated by the r24 residual lane the same day): OPEN. Narrowed residual of
[`proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016-RETIRED-20261024.md`](../../internal/fixed-bugs/proxy-define-row-cpu-is-class-file-api-java-and-define-epoch-flushes-20261016-RETIRED-20261024.md),
which is retired: every item it named is fixed, flipped, measured neutral, or carried here.

## What the row is

`tools/probes/Zs9Nd8ProxyCost.java`: per iteration, a fresh `ClassLoader`, `Proxy.getProxyClass(loader,
Runnable.class)` (a dynamic `jdk.proxyN` module, a `ProxyGenerator` class file through the ClassFile API, a define, a
`getDeclaredConstructors` link), then 512 bytes of garbage. Arms: `room` (the row), `define` (fresh loader + one
prebuilt class), `generate` (the ClassFile API only).

## Where it stands (Linux host 64.177.43.150, load 35-50, `taskset -c 8-15`, main-thread CPU µs/define, 2 reps)

| arm | defines | HotSpot 25.0.4 | CratonVM dev `ec551c685e` | CratonVM r24 `kpc` |
|---|---|---|---|---|
| room | 3000 | 272 / 326 | 1344 / 2153 | 1290 / 1667 |
| room | 20000 | 124 / 138 | 1305 / 1941 | 1223 / 1425 |
| define | 3000 | 20 / 27 | 117 / 158 | 108 / 128 |
| define | 20000 | 13 / 18 | 129 / 149 | 88 / 151 |
| generate | 3000 | 37 / 44 | 114 / 141 | 117 / 122 |
| generate | 20000 | 14 / 20 | 65 / 77 | 67 / 120 |

The host is shared and loaded, so single rows swing ~1.5x; the ratio to HotSpot is the stable reading: `room` 5-10x,
`define` 5-8x, `generate` 3-6x.

## What r24 removed (no longer on any profile)

At 400 000 defines (`define` arm, frame-pointer build, `perf record -g`), three super-linear terms took 47 % of the main
thread; all three are gone:

- `VtableManager::defer_dispatch` + its `FxHashMap` probes (28 %): the lazy-dispatch key put the class id in the high
  half, and Fx keeps the low bits, so every class's method `i` shared one probe chain.
- `ClassStore::direct_subtypes_of_loaders` (19 %): the miss memo was cleared by every define's own `set_superclass`.
- after those, `MemoRows::namespace_of` / `loader_object_for_namespace_id_in` (21 %, linear loader tables) and
  `ClassStore::withdraw_name`'s `retain` over one shared name's id list (2.9 %).

## What is left

1. **Compiled ClassFile-API code (generate arm, and ~38 % of `room`).** Every ClassFile-API body reaches the optimizing
   tier (r23 waves 8-10). What remains is code quality: dense stack-home traffic (every value homed and reloaded, the
   dead-home clears at every armed site; see the CF residual page,
   [`cf-stage-rows-residual-...-RETIRED-20261024.md`](../../internal/fixed-bugs/cf-stage-rows-residual-is-the-opt-in-handler-family-and-per-site-dead-home-clears-20261024-RETIRED-20261024.md),
   whose `CRATONVM_JIT_IR_OTHER_SITE_CLEANS` removed most re-zeroing), uninlined getters through the MIC
   (`AbstractPoolEntry.index()` and friends, proposal 4f), and `Class.isInstance` with a constant non-exact mirror
   (`ClassReaderImpl.checkType`, design in the retired page's wave 8e).
2. **A flat VM-side cost (45 % of the `room` main thread, ~0.65 % for the largest single symbol).** The frame-pointer
   profile (`perf report --comm main-vm --dso cvm-*`) has no hot spot: invocation doors (`jit_invoke_dispatch`,
   `jit_invoke_virtual_mic_body`, `try_lean_native_door`, `safe_native_call_impl`, `invoke_on_class_shared_inner`),
   `record_object_ref_payload_slow`, ZGC address screens (`is_object_address`, `ZObjectStarts::contains`),
   `StringPool::intern_arc`, `resolve_field_slot_by_name_cached`, the verifier (`verify_method`,
   `verify_instruction_with`) and mimalloc. HotSpot pays the same Java upcalls (`loadClass`, `findLoadedClass`,
   `Class.forName`) at ~1/10 of the cost. Closing this is the per-call cost of the VM's dispatch and native doors, not
   one defect.
3. **Link-time verification of every proxy class** (`getDeclaredConstructors0` -> link, ~8 % of `room`): HotSpot
   verifies proxies too (`BytecodeVerificationRemote`), so the item is the verifier's own cost.

## r24 residual lane (2026-10-24): what moved, what did not

Measured on the same host (load 15-25, `taskset -c 8-15`, `-XX:+UseZGC -Xmx256m`, main-thread CPU µs/define, min of
3-4 interleaved runs).

- **Fixed: a second quadratic scan in the define path.** `builtin_image_class_has_direct_supertype` (the built-in
  `findLoadedClass` rule, asked on every fresh loader) read every direct subtype of a JDK root on each call -- every
  class defined so far. At 30 000 defines it was 10 % of the `define` arm's main thread
  (`ClassStore::direct_subtype_ids` + `map_try_fold` over `&u32`). `ClassStore::direct_subtype_any_since` now
  remembers a miss per `(root, predicate key)` as the r23 `direct_subtypes_of_loaders` memo does, dropped by the
  same edge movers (`CRATONVM_DIRECT_SUBTYPE_ANY_MEMO`, default ON, `=0` kills; unit test
  `r24_direct_subtype_any_memo_answers_like_the_full_scan`). `define 30000`: 279 -> 238 (HotSpot 19); the arm's
  profile is flat again (no symbol above 1.5 %), and its per-define cost no longer grows with the count.
- **Measured neutral here: the CF handler family, the dead-home cleaning rule, the frozen prune floor and the
  bound-site splice cache** (all default ON now): `room 3000` 1499 against 1526, `generate 3000` 111 against 107,
  `define 3000` 132 against 132 (HotSpot 184 / 25 / 16).
- **Corrections to this page.** The constant-mirror `isInstance` is not exact-class only any more: the IR prefix's
  subtype arm (`CRATONVM_JIT_CLASS_IS_INSTANCE_SUBTYPE_INLINE`, default ON) scans interfaces too, so wave 8e's fold
  would replace an inline answer, not a call. The probe's own phase histogram is biased (`getStackTrace`
  samples): it reports the fresh `ClassLoader.<init>` at 15 % of CratonVM's samples, `perf` at 1.5 %. The
  main-thread compiles on the `room` profile (4.9 % at 8000 defines, `try_jit_compile_callee_slow`) are warm-up:
  1748 compiles at 1000 defines, 1845 at 4000, none of a per-define class. `AbstractPoolEntry.index` is compiled 27
  times (receiver-keyed inherited bodies), bounded.
- **Lowering the C2 bar to HotSpot's 5 000 invocations** (`CRATONVM_TIER_C2_THRESHOLD=5000`; the default is 20 000,
  so once-per-define methods such as `ProxyGenerator.<init>` stay C1 for a 3000-define row) did not move `room 3000`
  (1517-1702 default, 1671-2937 at 5000, 1896-2032 at 2000).
- **Inclusive `room` profile** (8000 defines, frame-pointer build): ClassFile-API generation 30 %,
  `ProxyBuilder.<init>` 11 %, `defineClass1` 10 %, `proxyClassContext` 8 %, `getDeclaredConstructors` + link 8 %,
  dynamic module 7 %; Java self time is 22 % C2 and 9 % C1.
- **Extending the CHA splice to `invokeinterface`** (item 1's getters) is not a watermark change alone: the verifier
  does not check an `invokeinterface` receiver, so the splice needs a subclass test of the selected body's declaring
  class beside the watermark (sound shape: walk that class's subtree, guard `receiver instanceof D && id <
  watermark`). Not built: the r23 triage measured the invokevirtual CHA splice neutral on this row.

## What would close it

The row reaching about 2x HotSpot. The next steps, in the order the profile ranks them:

- a call-graph profile of `room` on a frame-pointer build (`RUSTFLAGS=-C force-frame-pointers=yes`; JIT frames keep
  RBP chains, so `perf record -g` unwinds through them, unlike DWARF), then the per-door cost of
  `jit_invoke_virtual_mic_body` and `try_lean_native_door` on the ClassFile-API bodies;
- the getters' inlining (proposal 4f / `CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE` covers `invokevirtual` only, not
  `invokeinterface` through `PoolEntry`);
- the constant-mirror `isInstance` fold.

## Measuring

`Zs9Nd8ProxyCost <arm> <n>` with `-XX:+UseZGC -Xmx256m`, the main-thread CPU line. On a shared host measure CPU, not
wall, and interleave arms. The `define` arm at 400 000 defines is the scaling check: its profile must stay free of any
term that grows with the define count.
