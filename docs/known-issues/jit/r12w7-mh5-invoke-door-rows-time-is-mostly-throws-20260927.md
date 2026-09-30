# `R12Hunt4MhInvokeForms` rows: 16 s against 0.76 s -- what the doors cost and what the throws cost

Status: OPEN (measured in round 13: a ~5 us per-call door floor, not the throws, is ~75% of `rows`; the next split is `R13Irexc2MhFloor`)
Area: `native-builtins/src/lang_invoke.rs` (the `invoke` / `invokeExact` /
`invokeWithArguments` / `bindTo` natives); the VM's exception construction for an `Err`
returned by a native (`vm/src/runtime/exceptions.rs`, not owned by lane mh5)
Severity: MEDIUM (performance; no wrong answer)
Found by: round 12 wave 7 lane mh5 (reading; no run was possible in this lane)

## What the probe measures

`R12Hunt4MhInvokeForms.rows(i)`, 30 000 times, makes about twenty method-handle calls and
THROWS about 6.5 exceptions per iteration: F1 `bindTo(Integer)` (CCE), F3
`invokeWithArguments(null)` (NPE), the counted `Long` row (CCE), F4 / F4b / F5 (a CCE every
other iteration each), F6 (NPE), F7 (CCE) -- about 195 000 throws in the phase. HotSpot's
759 ms is then about 25 us per iteration, which is roughly what 6.5 stack-trace-filling
throws cost there (`-XX:-OmitStackTraceInFastThrow` does not apply to these; they are
explicit `new`s in the JDK's adapters). CratonVM's 16 080 ms is about 530 us per
iteration: if the throws dominate there too, that is about 80 us per throw. The probe
cannot tell the doors' cost from the throws' cost, and neither can reading.

## What wave 7 cut in the doors (by reading)

* The declared type (`mh_declared_descriptor`: two by-name field reads, then a
  mirror-to-name resolution and a `String` per parameter) was rendered up to four times per
  `invoke` (door, `invoke_declared_type_refusal`, `invoke_narrowing_arg_refusal`,
  `adapter_entry_args`), twice per `invokeExact` (`exact_call_site_refusal`, door) and twice
  per `invokeWithArguments` (the cast refusal, door). It is read once per call now and
  passed down.
* `reference_arg_admitted` answers an exact class match from one `Arc<str>` clone before the
  unique-name lookup, the value's name `String`, the stub check and the hierarchy walks; the
  doors ask it for every reference argument, declared and leaf.
* `direct_return_boxes` (every primitive-returning leaf dispatch) no longer renders the
  whole declared type to read its return: `mh_type_returns_reference`.

## What is left, in the order to measure it

1. **The throw itself.** Each refusal is an `Err(RuntimeError::...)` the VM turns into a
   Java exception (allocation, `<init>`, stack-trace capture across interpreted, compiled
   and native frames). Exception-THROW-path performance is out of scope for round 12's
   lanes (common brief, rule 9); this page names it so the owner of that path sees it.
   `R12Mh5DoorCost` phases `java-cce` / `java-npe` are the same throws with no method handle
   involved: if their ratio to HotSpot matches `invoke-cce` / `iwa-npe`, the doors are not
   the cost.
2. **`bindTo`'s allocations.** Every `bindTo` mints a handle (`alloc_method_handle`: the
   object, three `String`s, a `MethodType`) and then a second `MethodType` for the dropped
   type; each `MethodType` brings a `MethodTypeForm` and two fabricated cache arrays of 16
   and 64 references (`populate_method_type_form`). About 1.5 KB and a dozen allocations per
   bind, where HotSpot allocates one `BoundMethodHandle`. Phase `bindto-invoke`. A per-VM
   descriptor-keyed `MethodType` cache would remove all but the handle (proposal HW7-2).
3. **Per-dispatch string decoding.** `mh_dispatch_body` decodes `MH_CLASS`, `MH_NAME` and
   `MH_DESC` from Java `String`s into Rust `String`s on every call, and the leaf arms parse
   the descriptor again (`split_descriptor_params`, `parse_descriptor_param_and_return`).
   Phases `exact-direct` / `invoke-direct`.
4. **The compiled caller's path into the door.** The JIT's per-site native cache refuses
   `invoke` by name, so a compiled `rows` reaches every door through the generic native
   tail. Compare the default arm with `--nojit`.

## How to confirm

Run `C:\craton\jitr12-probes\src\R12Mh5DoorCost.java` on HotSpot 25 and CratonVM (default,
`--nojit`): each phase prints `<phase> <N> ms` and a fixed `sum-<phase>` line. The phase
whose CratonVM/HotSpot ratio is largest is where the next cut goes; if it is `java-cce` /
`java-npe` as much as the method-handle phases, file it against the exception path.

## Round 13 wave 1 (lane mhffm)

No measurement was possible in this lane (no builds, no runs), and the page's own rule is to
measure `R12Mh5DoorCost` before cutting, so no door was changed for speed. Two things that bear on
the measurement landed nearby: `invoke_virtual_declared` (every virtual leaf) now asks
`declared_dispatch_selection` first (one class-manager read and a superclass walk by name; the
common shapes answer without a selection walk), and a stacked-stamp handle re-boxes arguments a
primitive middle link converts (`CRATONVM_MH_CAST_CHAIN_CONVERTS`). Neither is on
`R12Hunt4MhInvokeForms`' throw rows. The ordered measurement plan above still stands; the
candidate cuts (a per-VM descriptor-keyed `MethodType` cache for `bindTo`, a decoded
`(class, name, desc)` cache per handle) are proposals MF13-4 and MF13-5 in
`jit-r13-mhffm-proposals-RETIRED-20260929.md`.

## Round 13 wave 8 (lane irexc2): the measurement exists; the title is refuted; the next split

The step-1 measurement was already taken by the round-13 battery runs: `R12Mh5DoorCost` is in
the R12 set, so every `run13.sh` pass recorded it. Second-round phases (ms per 30 000 calls),
from `C:\craton\jitr13-probes\out\w8b-def\R12Mh5DoorCost.raw` (`w7m-def` within 10%) against
`C:\craton\jitr12-probes\ref12\R12Mh5DoorCost.txt` (HotSpot 25.0.3):

| phase | CratonVM w8b | HotSpot | reading |
|---|---:|---:|---|
| exact-direct | 148 | 0 | the door floor: ~4.9 us a call |
| invoke-direct | 154 | 0 | floor + ~0.2 us |
| invoke-boxed | 174 | 2 | floor + unboxing |
| iwa-array / iwa-list | 198 / 214 | 6 / 5 | floor + array/list unpack |
| invoke-virtual | 168 | 0 | floor |
| bindto-invoke | 293 | 4 | floor + ~4.8 us of `bindTo` allocation (step 2) |
| invoke-varargs | 189 | 7 | floor + collection |
| invoke-cce | 300 | 21 | floor + ~5 us throw |
| iwa-npe | 168 | 245 | CratonVM is FASTER |
| java-cce / java-npe | 216 / 192 | 75 / 60 | the VM's plain throw: ~2.9x / 3.2x HotSpot |

And `R12Hunt4MhInvokeForms rows`: 4616 ms (w8b) / 3861 ms (w7m) against HotSpot 726 ms, no longer
16 s. The model "20 door calls x ~5 us + 6.5 throws x ~4-7 us per iteration" gives ~3.8-4.4 s,
which matches. So:

- **The doors, not the throws, are ~75% of `rows`**: a fixed ~5 us per call on EVERY shape,
  including `invokeExact` of a static `(int)int` that does no conversion at all. HotSpot inlines
  these to ~0. This page's title ("mostly throws") is refuted.
- The throw cost is the VM's exception path (~3x HotSpot for a plain `checkcast` / NPE with no
  handle involved), the same inside and outside a door: not a door defect.
- `bindTo` adds about one more floor per call (step 2 / proposal HW7-2 stand).

Nothing was changed for speed this wave: the floor is spread over the whole door
(`mh_invoke_exact_door`: `mh_read_desc`, `MH_KIND`, `mh_declared_descriptor` = a by-name `type`
read plus a mirror-to-name render per parameter, `exact_call_site_refusal`,
`null_receiver_refusal`, `invoke_exact_inner_cast_refusal`, then `mh_dispatch_body`: three Java
`String` decodes, `is_downcall_handle`, `mh_is_varargs_collector`, `direct_return_boxes`,
`mh_entry_adapt`, the target resolution by name and the re-entry), and no single site is a
by-reading win worth its risk in a 33k-line file. The split below says which part to cut.

**Next measurement** (orchestrator): `C:\craton\jitr13-probes\src\R13Irexc2MhFloor.java` (new):
`native-floor` (a plain registered native, no handle), `lambda`, `reflect` (`Method.invoke`),
`exact-0` (`()int`, the smallest door), `exact-1`, `exact-obj` (`(Object)Object`, no boxing),
`exact-virtual`, `exact-bound` (bound once, outside the loop), `bind-each`, `invoke-1`.
```
cd /c/craton/jitr13-probes; ./prep13.sh R13Irexc2MhFloor
PROBES="R13Irexc2MhFloor R12Mh5DoorCost R12Hunt4MhInvokeForms" ./run13.sh $EXE irexc2-mh-def
PROBES="R13Irexc2MhFloor R12Mh5DoorCost R12Hunt4MhInvokeForms" ./run13.sh $EXE irexc2-mh-nojit --nojit
PROBES="R13Irexc2MhFloor" ./run13.sh $EXE irexc2-mh-compat --compatible
```
Read the second-round lines (`grep -v warm`):
- `exact-0 - native-floor` is the door's own fixed work. If it is most of the 5 us, the cut is a
  per-handle decoded record (class id, resolved target, declared descriptor, kind, the
  exact-signature verdict per call-site descriptor): proposal MF13-5 of
  `jit-r13-mhffm-proposals-RETIRED-20260929.md`, which removes the `String` decodes, the by-name `type` read and
  the per-call descriptor renders together.
- `native-floor` itself near 5 us (and `reflect` similar) means the cost is the native-call
  transition, not `lang_invoke.rs`: file it against the native call path.
- `default` much slower than `--nojit` on the same phase means the compiled caller's generic
  native tail (step 4) is the cost.
- `exact-bound` near `exact-0` and `bind-each` ~2x it confirms step 2 (HW7-2).
Status stays OPEN (performance; the split decides the next cut).

## Round 13 wave 10 (lane callcost4): the split was already measured; the floor is cut in three places

**The split.** `R13Irexc2MhFloor` has run in every round-13 battery since w8d
(`C:\craton\jitr13-probes\out\w9a-*\R13Irexc2MhFloor.raw`). Second-round lines, w9a default
arm (ms per 30 000 calls; HotSpot 25.0.3 reads 0-1 on every row but `reflect` 6, `bind-each` 3):
`native-floor` 4, `lambda` 1, `reflect` 158, `exact-0` 175, `exact-1` 208, `exact-obj` 216,
`exact-virtual` 233, `exact-bound` 176, `bind-each` 937, `invoke-1` 484. The other arms
(thr1, c2always, c2never, nle0, compat) agree within the ~3x rep noise; g1 is ~1.5-2x slower
on every door row (the door allocates: the result box). Reading the page's own rules:

- `exact-0 - native-floor` is ~5.7 us: the door's fixed work, not the native transition
  (`native-floor` is a site-cached registered native at 0.13 us).
- `reflect` ~ `exact-0` does NOT say the transition is shared: under `--jdk-only`
  `Method.invoke` is real JDK bytecode (`DirectMethodHandleAccessor`) that calls
  `invokeExact` itself, so the `reflect` row IS a door row.
- There is no `--nojit` arm in the battery; by reading instead (below), a compiled caller
  reaches the door through the generic by-name route, which a plain native does not.

**Where the 5 us goes, by reading** (a compiled `(int) ZERO.invokeExact()`):

1. *The compiled caller's path into the door.* `jit_invoke_virtual_mic` asks the native site
   cache first, which refused the `invoke*` names outright (`site_name_is_special_cased`), so
   every call fell to `invoke_or_native`'s cascade, then to `invoke_on_class_shared_inner`
   (~5 450 lines of by-name arms) to reach the signature-polymorphic block that runs the door.
2. *The door itself.* `MH_DESC` decoded three times, the declared `type` rendered three times
   (`mh_declared_descriptor`, `invoke_exact_inner_cast_refusal`, `direct_return_boxes`), the
   argument list copied four times with a pin/unpin pass, and a primitive result boxed through
   `Integer.valueOf` only for `unbox_poly_return_checked` to unbox it (a class-manager read and
   a class-name `String`, twice under the strict `invokeExact` check).
3. *The door's leaf call.* `invoke_static_settling`'s settled arm runs
   `invoke_on_class_shared_inner` AGAIN, by name, before its tail runs `interpreter::execute`.

**Landed (both default ON, each with its own switch):**

- (2) **The direct lane** (`native-builtins/src/lang_invoke.rs` `mh_direct_lane`,
  `CRATONVM_MH_DIRECT_LANE`): a `findStatic` or `findVirtual` handle (bound or not) with no
  marking bit, no cast chain, no `specialCaller`, whose declared `type` is its own member type
  and is the call site's descriptor, called with raw primitives / references and no array
  parameter, is served by the leaf call alone. Every refusal the ordinary door could still
  raise for such a call is asked by the same function in the same order
  (`invoke_reference_cast_refusal_with` and `mh_entry_adapt` for `invoke`,
  `leaf_receiver_refusal`, `leaf_reference_refusal`); everything that cannot fire is skipped;
  a shape it does not model is declined before any effect. The equivalence argument is on
  `mh_direct_lane`'s doc comment, step by step. With `CRATONVM_MH_DIRECT_LANE_RAW_RETURN` (default ON) a
  primitive of the call site's own return type goes back raw: every by-name route into these
  natives passes the result through `unbox_poly_return_checked`, which keeps a raw primitive
  and unboxes the door's box to the same `Value`. Tests: `r13w10_callcost4_direct_lane_tests`
  (virtual, bound virtual, static and `void` answers; every decline; the refusals as the door
  words them; the explicit-cast scope restored; the raw-return rule).
- (1) **The native site cache serves `MethodHandle.invoke` / `invokeExact` on the VM's own
  carrier** (`vm/src/jit/helpers.rs` `method_handle_carrier_site` /
  `resolve_method_handle_carrier_native`, `CRATONVM_JIT_SITE_CACHE_MH_INVOKE`): for a receiver
  whose class IS `java/lang/invoke/MethodHandle` every arm before the signature-polymorphic
  block declines, so the entry is that block's ending: the erased native, the call-site
  descriptor armed at dispatch (`poly_call_site::arm`), the result through
  `unbox_poly_return_checked`. The receiver-class guard keeps real JDK `MethodHandle`
  subclasses on the by-name route; such an entry never arms the `VarHandle` value cell.
  Test: `only_the_method_handle_carrier_opens_the_invoke_family`.

**Filed, not applied (not this lane's files):** (3) as an exact patch,
`r13w10-callcost4-static-callee-memo-patch-FIXED-20260929.md`: the static half of the
native->Java callback memo (`runtime::native_callee_memo`), keyed on (owner, first-argument
class, name, descriptor), so a settled `findStatic` handle's leaf call is the tail's own
`interpreter::execute` after the first call.

**What is left:** HW7-2 (`bindTo`'s allocations: `bind-each` - `exact-bound` is ~25 us a
call here), the decoded per-handle record (MF13-5: the lane still decodes three `String`s and
renders `type` once per call), the throw path (`java-cce` / `java-npe` ~3x HotSpot, not a door
defect), and adapter kinds (`asType` copies, collectors, combinators), which keep the ordinary
door. Proposals CC4-1..CC4-6 in `jit-r13-callcost4-proposals-RETIRED-20260929.md`.

**Measure** (orchestrator; `$EXE` = the wave binary):
```
cd /c/craton/jitr13-probes; ./prep13.sh R13Callcost4MhDirect
for r in 1 2 3; do for arm in def lane0 site0 raw0; do e=; case $arm in
  lane0) e="CRATONVM_MH_DIRECT_LANE=0";; site0) e="CRATONVM_JIT_SITE_CACHE_MH_INVOKE=0";;
  raw0) e="CRATONVM_MH_DIRECT_LANE_RAW_RETURN=0";; esac
  env $e PROBES="R13Irexc2MhFloor R13Callcost4MhDirect R12Mh5DoorCost R12Hunt4MhInvokeForms" ./run13.sh $EXE cc4-$arm-r$r
done; done
```
Answers (`sum-*`, `bad 0`) identical in every arm; read the second-round timing lines. Expected:
`exact-*` / `invoke-1` / `exact-bound` well below the w9a figures in `def`, back near them in
`lane0 site0`. Also `--compatible` and `--nojit` once each (the interpreter reaches the door
through its stackless path, so `site0` does not apply there).
Status stays OPEN (performance; measure, then decide the next cut from the new split).

## Round 13 wave 11 (lane callcost5)

**Measured (w10f, default arm, second round, ms per 30 000 calls; w9a in brackets).** From
`C:\craton\jitr13-probes\out\w10f-def\R13Irexc2MhFloor.raw`:

| row | w10f | w9a |
|---|---:|---:|
| `native-floor` | 4 | 4 |
| `reflect` | 107 | 158 |
| `exact-0` | 70 | 175 |
| `exact-1` | 78 | 208 |
| `exact-obj` | 79 | 216 |
| `exact-virtual` | 94 | 233 |
| `exact-bound` | 66 | 176 |
| `bind-each` | 230 | 937 |
| `invoke-1` | 93 | 484 |

Wave 10's direct lane and the site cache took the `static final` door floor from ~5.8 us to
~2.3-2.6 us a call. What is left of it is mostly (3) above, the leaf's by-name re-resolution.

**Landed: (3), the static-callee memo.** The exact patch
`r13w10-callcost4-static-callee-memo-patch-FIXED-20260929.md` is applied (switch
`CRATONVM_NATIVE_CALLBACK_MEMO_STATIC`, default ON). A settled `findStatic` handle's leaf call is
now the ordinary tail's own `interpreter::execute` from the second call on. It no longer runs
`invoke_on_class_shared_inner`'s by-name cascade (two class-manager reads with a `String` each,
`find_method_recursive`, three registry probes, the FFM / Path / SSL / proxy arms,
`resolve_dispatch`). Expected: `exact-0`, `exact-1`, `exact-obj` and `R13Callcost4MhDirect`'s
`static-*` rows drop again, while `exact-virtual`, `exact-bound` and the virtual rows stay where
they are (their leaf is `invoke_virtual`, which already had the virtual memo).

**An open question the numbers raise.** In the same binary and arm, `R13Callcost4MhDirect`'s
`static-int` is 192 ms per 30 000 calls (6.4 us). That is the pre-lane floor, against
`R13Irexc2MhFloor`'s `exact-1` at 78 ms for the same `(int)int` `findStatic` `invokeExact` shape.
`static-subint` (4 calls per iteration, 793 ms) agrees at ~6.6 us a call. By reading, nothing
in the handle separates the two probes: the targets are `public static`, the handles come from
`<clinit>`, `asType` clones (`mh_with_stamped_type` -> `mh_clone_handle`), and `bindTo` mints.
So the direct lane may not engage in `R13Callcost4MhDirect` at all. First step:
`PROBES=R13Callcost4MhDirect` with `CRATONVM_MH_DIRECT_LANE=0` against the default. If the
`static-*` rows do not move, the lane declines there, and `CRATONVM_DBG_MH_DISPATCH=1` (the
lane stands aside under it, so this is the ordinary door's view) plus a one-off
`eprintln!` in `mh_direct_lane_shape`'s `None` returns will name the declining conjunct. The
probe's static phases run AFTER `mh-recursion` in round 1 and first in round 2, which is the
only ordering difference.

**What is left:** HW7-2 (`bindTo`'s allocations: `bind-each` - `exact-bound` is still ~5.5 us a
call), the decoded per-handle record (MF13-5 / CC4-4), constant handles in compiled code (CC4-1,
the only route to HotSpot's ~0), the throw path (not a door defect), and the question above.
**Measure** (orchestrator): `R13Irexc2MhFloor`, `R13Callcost4MhDirect` and
`R13Callcost5StaticMemo` (new), default against `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC=0`,
interleaved, three reps. Also `R13Callcost4MhDirect` with `CRATONVM_MH_DIRECT_LANE=0` once.
Status stays OPEN (performance).

## Round 14 wave 1 (lane calls)

**The wave-11 open question is answered by the round-13 battery: the direct lane engages in
`R13Callcost4MhDirect`.** The question was why `static-int` (192 ms per 30 000 calls at w10f) sat
at the pre-lane floor while `R13Irexc2MhFloor`'s `exact-1` (same `(int)int` `findStatic`
`invokeExact` shape) read 78 ms. The w10f binary predates the static-callee memo (landed in wave
11). With it, second-round lines (ms per 30 000 calls), `C:\craton\jitr13-probes\out\w12a-def`:

| row | `R13Callcost4MhDirect` | `R13Irexc2MhFloor` |
|---|---:|---:|
| static-int / exact-1 | 77 | 78 |
| static-void, static-float, lazy-init | 74, 75, 73 | -- |
| virtual / exact-virtual | 93 | 94 |
| bound / exact-bound | 80 | 79 |

So the two probes agree to within 2% on every shared shape, and the ~2.2-2.6 us a call both show
is the lane's floor, not a declined lane. The gap at w10f was the memo, not the lane. (w13a reads
1.2-2.4x higher on both probes and on `native-floor`-free rows alike; that run was on a loaded
machine and moves every row, not the door rows only.) CC5-2's decline census is therefore not needed
to answer this question; it stays a proposal for the next door change.

**What the ~2.5 us floor still is, by reading** (a compiled `(int) TWICE.invokeExact(x)`): the
compiled caller's MIC-helper entry and native site cache (`vm/src/jit/helpers.rs`,
`method_handle_carrier_site`), the native call transition with its argument `Vec` and pin pass,
`mh_direct_lane_shape` (three by-name field reads and three Java `String` decodes --
`mh_read_desc` / `mh_read_class` / `mh_read_name` -- plus `split_descriptor_params` and
`mh_declared_descriptor`'s mirror render, every call), `invoke_static_settling`'s memo hit, and
`interpreter::execute` of the leaf. HotSpot inlines the whole chain to the leaf's body.

**What is left, ranked** (unchanged in substance, sharper in order):
1. MF13-5 / CC4-4, the decoded per-handle record: the three `String` decodes and the declared-type
   render are pure functions of an immutable handle and are paid on every call; a per-handle
   (weak-keyed, per-VM) record of `(kind, class, name, desc, params, declared == own type)` makes
   `mh_direct_lane_shape` a lookup. `native-builtins/src/lang_invoke.rs` (not this lane's file; lane
   ffm is editing it this wave).
2. CC4-1, constant handles in compiled code: a `static final` `MethodHandle` whose direct-lane
   shape is fixed can be called as its leaf from the compiled caller (a guarded direct call on the
   handle's identity), the only route to HotSpot's ~0. Large; proposal C14-2 in
   `jit-r14-calls-proposals.md` gives the first cut.
3. HW7-2 (`bindTo`'s allocations: `bind-each` - `exact-bound` ~8 us at w12a).
Status stays OPEN (performance).
