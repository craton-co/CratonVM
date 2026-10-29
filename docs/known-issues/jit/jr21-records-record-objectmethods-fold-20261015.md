# Record `hashCode` / `equals` as an IR field-wise fold (IN16-6)

> **j27 wave 8 (lane W8DSP, 2026-10-28): REVIEWED (no change), PENDING HOST VERIFICATION.** Re-read wave 5's
> `jit/src/ir/record_fold_shortcuts.rs` against the fold in `jit/src/ir.rs`: the `String.hash` slot
> (`DirectHelperTable::string_hash_field`, the VM's `find_own_field_by_descriptor` index =
> `first_field_index + position`) is the same instance-slot index space the fold's component loads and the
> `0xb4` arm use, so `record_fold_load(.., slot, b'I')` reads `hash`; the null arm keeps the memory token from
> before the load, the hit arm the load's, the miss arm the call's, and every arm joins through one `Merge` +
> memory / value `Phi` (`record_fold_join`), as the fold's own exits do; `Op::Call` is not a control node here
> (the call is pinned under the miss projection), matching `record_fold_call`'s other uses. The identity
> shortcut skips only an `Objects.equals` that would return `true` before running Java. No defect. Wave 5's
> verify recipe stands; the remaining items (RECORDS21-1 / -2 / -4, the `String.equals` half of -3) are
> unchanged proposals.

> **j27 wave 5 (lane RECORDS, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** The fold itself
> (IN16-6) was already in code, default ON (wave 1 block below); this wave builds the hash/equals
> half of RECORDS21-3 in front of the fold's per-reference `Objects` calls, in a new module
> `jit/src/ir/record_fold_shortcuts.rs`:
> * `hashCode`, a `Ljava/lang/String;` component `s`: `s == null ? 0 : (s.hash != 0 ? s.hash :
>   Objects.hashCode(s))`. Switch `CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH` (perf, default ON,
>   `=0` keeps the call). The VM marks String-typed components
>   (`record_fold_string_components`, `jit_bridge.rs`, new field `IndyRecordFold::string_components`);
>   `lib.rs` fills `string_hash_slot` per compile from `DirectHelperTable::string_hash_field` (the slot
>   the `Object.hashCode()` String prefix already reads) through `ir::record_fold_admit_string_hash`,
>   which reads the switch.
> * `equals`, every reference component pair: `a == b ? true : Objects.equals(a, b)`. Switch
>   `CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY` (perf, default ON, `=0` keeps the call), read per
>   fold build.
> * Exact because `Objects.hashCode` / `String.hashCode` return the non-zero cached `hash` first and
>   `Objects.equals` tests `a == b` before any `equals` runs; neither skipped call could have thrown.
>   The equals tail now joins through the shared `IrBuilder::record_fold_join`.
> * Not built: RECORDS21-1 (fold inside a splice), RECORDS21-2 (`toString`; the bridge's frame-free
>   arm still answers it), RECORDS21-4 (single-pass tier), and the `String.equals` half of
>   RECORDS21-3 for distinct equal strings. The cached-hash read has no compact row (`Graph::field_rows`),
>   so a COMPACT `String` takes the checked field helper, as the record's own component reads do.
>
> Unit tests (`jit` crate): `ir::record_fold_shortcuts_tests` (5 tests) and the unchanged
> `ir::jr21_records_fold_tests`. Probe `tools/probes/jit/j27/J27RecordFoldRows.java`; HotSpot 25
> expected stdout (hand-derived from JDK `ObjectMethods` / `Objects` semantics):
> ```
> fact s-hash-mismatches 0
> fact s-equals-mismatches 0
> fact c-equals-shared 200000 calls=0
> fact c-equals-distinct 200000 calls=200000
> fact c-hash 1400000 calls=200000
> fact boom-shared-equals true
> fact boom-distinct-equals java.lang.RuntimeException:boom-eq
> fact s-hash-ab 3105
> fact s-hash-null-b 98
> fact s-hash-empty 0
> fact s-hash-nulls 0
> fact zero-hash-string 0
> fact s-hash-zero-hash-string 97
> fact s-hash-fresh-zero-hash-string 97
> fact n-hash-nulls 961
> fact n-hash-ab 4066
> fact s-equals-literals true
> fact s-equals-copies true
> fact s-equals-differ false
> fact s-equals-null-null true
> fact s-equals-null-value false
> fact s-equals-value-null false
> fact e-equals-same true
> fact e-equals-other false
> fact e-hash-formula true
> ```
> Verify recipe: run HotSpot 25 first and diff against the block above; then CratonVM `--jdk-only` and
> `--compatible`, each on the default arm, `CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH=0`,
> `CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY=0` and `CRATONVM_JIT_IR_INDY_RECORD_FOLD=0`; stdout must
> equal HotSpot on all arms. `CRATONVM_DBG_JITC=1` on the default arm must show
> `[ir] indy RECORD FOLD at bytecode pc 1 in ...$S.hashCode ... (string-hash slot Some(..), 2 String
> component(s))`. Timing: `-Dj27.time=1` rows `hash-strings`, `equals-shared-strings`,
> `equals-copied-strings` (medians of the 5 reps), default vs each `=0`; if a shortcut loses on its row,
> turn that switch OFF (keep it, per the switch rule) and note it here. Also re-run the wave 1 recipe
> (`T1W8RlStrRecordFold` / `R21RecordsRows`) on the default arm, since the fold's graph changed.

> **j27 wave 1 (lane MHINDY, 2026-10-28): NOT FIXED (no code change needed), PENDING HOST VERIFICATION.**
> Drift resolved by reading the code: the fold is **default ON**. The VM gate
> `jit_ir_indy_record_fold_enabled` (`vm/src/runtime/interpreter/jit_bridge.rs`) reads
> `runtime_flag_default_on("CRATONVM_JIT_IR_INDY_RECORD_FOLD")` (kill `=0`), as since `44f0382076`
> (the formatted 3-way re-land of round 21's code), and the registry row agrees (`default-on`,
> token `ir-indy-record-fold`). The t1 wave 8 block below ("opt-in, `runtime_flag_on`") describes the
> t1 RL-STR re-land that `44f0382076` superseded; it is stale. Parity review of the fold
> (`IrBuilder::record_fold_hash` / `record_fold_equals` / `record_fold_float_bits` /
> `record_fold_double_bits`, `CpResolvers::indy_record_fold`) against JDK `ObjectMethods`, no
> divergence found: a null reference component hashes through a CALLED `Objects.hashCode` (0) and
> compares through `Objects.equals` (null-safe), so the component's own `hashCode`/`equals` and its
> exception frames are HotSpot's; an array component goes through the same calls (identity
> hash/equality, as `Object` methods); `float` hashes `floatToIntBits` (canonical NaN
> `0x7fc00000`), `double` `doubleToLongBits` then `(int)(v ^ v>>>32)`; `float`/`double` equality is
> canonical-bits equality (= `Float/Double.compare == 0`: `NaN == NaN`, `0.0 != -0.0`); `boolean`
> 1231/1237; getters in order for `hashCode` (`h*31 + c`), LAST first for `equals`, after
> `this == o` and the exact-class test (null and arrays false); a zero-component record answers 0 /
> `isInstance` as the bootstrap's empty fold does. Not checkable by reading: whether JDK 25's
> `ObjectMethods` still has exactly that combiner and order -- the host run of
> `tools/probes/t1/T1W8RlStrRecordFold.java` / `R21RecordsRows` against HotSpot 25 on the default arm
> and on `=0` (both modes) settles it, with `CRATONVM_DBG_JITC=1` showing `[ir] indy RECORD FOLD`
> on the default arm only.

> **t1 status (2026-10-26, wave 8, lane RL-STR):** the code was dropped at merge `624fae0b49`. RE-LANDED IN CODE
> as **opt-in** (`CRATONVM_JIT_IR_INDY_RECORD_FOLD=1`; the VM resolver reads `runtime_flag_on`), since round 21
> never measured it on the host. PENDING HOST VERIFICATION: `tools/probes/t1/T1W8RlStrRecordFold.java` facts =
> HotSpot 25 with `=1` and unset, both modes, Gen / G1 / ZGC; then `-Dt1w8.time=1` rows `hashP` / `equalsR`
> (and `R21RecordsRows`), medians of 5, before any flip.

Status: PENDING HOST MEASUREMENT (code is in behind `CRATONVM_JIT_IR_INDY_RECORD_FOLD`, default ON;
JIT round 21 wave 17, lane records). Correctness facts must also be checked against HotSpot 25 (below).

Re-land (20261026): merge `624fae0b49` had dropped this round-21 code from dev; it is back on dev since `44f0382076` (formatted 3-way re-land, `docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md`). The status above describes the code again; its host verification is recorded there.

Area: JIT optimizing tier (`jit/src/ir.rs` builder, `jit/src/lib.rs` indy plan, `jit/src/ir_lower.rs`
post-call exits), VM compile door (`vm/src/runtime/interpreter/jit_bridge.rs`).
Found by: r16 indy lane (IN16-6), carried by the r18 triage and the r21 retriage
(`jr21-r18-backlog-retriage-20261015.md` section 2.2).

## The gap

A record's javac-generated `hashCode()I` / `equals(Ljava/lang/Object;)Z` is `aload_0; [aload_1;]
invokedynamic ObjectMethods.bootstrap; ireturn`. In the optimizing tier the `invokedynamic` was an
`Op::IndyBridge` call: the bridge's frame-free record arm (`CRATONVM_JIT_INDY_RECORD_FAST`) walks the
record natively per call (`record_object_method_value`, a `NativeContextImpl` per component, Java
dispatch per non-String reference). `R16IndyRecord hashP`: 768 ns here vs 2 ns on HotSpot, which
inlines the `MethodHandle` chain.

## Round 21 change

* **VM** (`CpResolvers::indy_record_fold`, new resolver `CompileRequest::cp_indy_record_fold_resolver`):
  answers `(IndyRecordFold { equals, record_class_id, components: [(field index, tag)] }, Objects class id)`
  only for the record class's OWN generated body (`Class::generated_record_object_methods`: `final`
  record, exact body shape, `ObjectMethods.bootstrap`, javac's canonical getter list), the
  `invokedynamic` at bci 1 (`hashCode`) / 2 (`equals`), the site bridged (bootstrap resolved), each
  getter a `REF_getField` resolved exactly as a `getfield` of its `Fieldref` (`jit_resolve_field_site`,
  kind and tag checks), declared by the class. Switch `CRATONVM_JIT_IR_INDY_RECORD_FOLD` read here
  (`jit_ir_indy_record_fold_enabled`), per compile.
* **JIT plan** (`ir_indy_record_fold_plan_into`, `lib.rs`): the site joins the indy plan as an
  `invokestatic java/util/Objects.hashCode(Object)I` / `equals(Object,Object)Z` (the call the handle
  chain makes per reference component), so that call gets its `invoke_info` row / direct bind. It is
  excluded from the pseudo-opcode map (the `0xba` arm builds it) and from splice planning
  (`IrIndyDirectPlan::is_record_fold_pc`). `IrBuilder::set_indy_record_folds` hands the rows over.
* **Builder** (`IrBuilder::try_indy_record_fold`, first in the `0xba` arm, acceptance evidence):
  * `hashCode`: `h = 0; h = 31*h + hash(load(this, field))` in getter order; `I/S/B/C` the value,
    `Z` `1237 - 6*(v != 0)`, `J` `(int)(v ^ v>>>32)`, `F` `floatToIntBits` (raw bits, NaN canonicalised
    branch-free), `D` `doubleToLongBits` then the long hash, `L`/`[` an `Op::Call` of `Objects.hashCode`.
  * `equals`: `this == o -> true`; `!ExactClassIs(o, R) -> false` (null, array, other class: the
    `isInstance` of a final class); then the getters LAST first, each `!eq -> false` (`==` for the int
    category, `lcmp == 0`, canonical-bits compare for `F`/`D` = `Float/Double.compare == 0`,
    `Objects.equals(a, b)` call for references); all-equal -> true. One `Merge` + memory/value `Phi`.
  * Loads are the `0xb4` arm's `Op::Load` by field index; the pc goes to
    `Graph::synthetic_field_access_pcs` (no per-pc compact row) and to the new
    `Graph::record_fold_call_pcs`.
  * Refused (bridge kept, named under `CRATONVM_DBG_JITC` as `[ir] indy RECORD FOLD refused`): inside a
    splice, shape mismatch, no class id, > 32 components, a tag the tier cannot load (`J` without the
    long gate, `F`/`D` without the FP gate or `CRATONVM_JIT_IR_FP_RAW_BITS`), a reference component
    without a matching `Objects` row, no frame state, dead control, non-reference operands.
* **Lowering** (`ir_lower::post_call_exit_state`): no post-call exit after a call at a
  `record_fold_call_pcs` pc -- that exit resumes after the `invokedynamic` with the call's value as the
  instruction's result, which a component's hash is not.

Why it is exact: JDK 25 `ObjectMethods.makeHashCode` (`hashCombiner(x, y) = x*31 + y`, accumulator
before the component), `makeEquals` (`guardWithTest(isSameObject, true, guardWithTest(isInstance,
acc, false))`, each getter's test outermost = last first), `hasher` / `equalator` per type, and
`Objects.hashCode` / `Objects.equals` for references (called, so the component's `hashCode`/`equals`
runs in the same order and an exception carries the same `Objects.*` frame HotSpot shows).

## How to verify

Probe `C:\craton\jr21-ssh\probes\R21RecordsRows.java`, arms: unset vs `CRATONVM_JIT_IR_INDY_RECORD_FOLD=0`,
each in `--jdk-only` (default) and `--compatible`. `CRATONVM_DBG_JITC=1` must show
`[indy-record-fold] hashCode()I bci=1 ...` and `[ir] indy RECORD FOLD at bytecode pc 1 in ...$Pt.hashCode`
on the default arm and none on `=0`.

* Facts: every `fact` line must equal HotSpot 25 on all four arms (the `*-cvm-*` one is VM-specific).
  `prims-hash-mismatches 0`, `prims-equals-mismatches 0`, `prims-formula true true true`,
  `array-hash-is-identity true` are self-checks; `nan-*`, `zero-*`, `boolean-hash`, `generic-*`,
  `order-*` (`order-hash abc:...`, `order-equals CBA:true CB:false C:false`,
  `order-equals-identical :true`), `boom-*` (messages and `boom-hash-trace` /
  `boom-equals-trace` frame lists, which include `Objects.hashCode` / `Objects.equals` on HotSpot) need
  the HotSpot reference.
* Time rows (ns per call): `hash-pt`, `hash-named`, `hash-prims`, `hash-nested`, `equals-pt`,
  `equals-pt-ne`, `equals-named`, `map-get-pt`. Decision: the default arm must beat `=0` on `hash-pt`
  and `equals-pt` by a wide margin (the bridge arm is hundreds of ns; the fold should be a compiled
  call plus a few ns) and must not lose on `hash-named` / `map-get-pt`. If it loses anywhere, turn the
  switch OFF and re-open this page.
* Unit tests: `jit` crate `ir::jr21_records_fold_tests` (4 tests: hash shape, primitive record without
  a call row, refusals, equals order/shape).

## Not done (see `jit-r21-records-proposals.md`)

`toString` (RECORDS21-2), the fold inside a splice so escape analysis can remove the record
(RECORDS21-1), a direct `String.hashCode` for `final`-typed reference components (RECORDS21-3), the
single-pass tier (RECORDS21-4).

## Wave 19 note (lane recnojit)

The interpreter / bridge side of the same methods lost `Objects.*` and the record frame under a
throwing component (`boom-*-trace` under `--nojit`). Fixed in `native-builtins/src/intrinsics/record.rs`;
see `jr21-records-nojit-component-frames-FIXED-20261026.md`. The IR fold itself was right and is unchanged.
