# JIT round 21 wave 23: correctness review of the opt-in perf switches before the flip A/B (lane review-optins)

Status: OPEN. 22 switches reviewed. Verdicts: 20 SAFE-TO-FLIP (one of them, #7, with a residual), 1 FLIP-AFTER-FIX,
1 KEEP-OFF until it is fixed.

- The KEEP-OFF is `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO`. It can hand out a pointer to a String that only the weak
  intern table holds, with no keep-alive barrier. Fixed in wave 24 (lane names, pending build): only strongly held
  answers are noted; see #13.
- The FLIP-AFTER-FIX is `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`. Its redefinition mark is taken after the
  class-manager guard is dropped. The fix moves one line.
- Smaller follow-ups (#6, #7, #8, #10, #14) are described at their sites below. None of them blocks a flip.
- Wave 24 (lane flipprep, pending build, no default changed) did several of them:
  - fixed #7 (the lost copy record) and #8 (the stale allowed row);
  - added the #14 / #15 arrays-never-compact test;
  - added the optional #2 JVMTI local-access gate;
  - wrote the probe rows for #3 / #20 and #21 / #22 (`R21FlipRows`).

  Each is recorded under its switch below.

Nothing was fixed: this lane may not edit `.rs` files.

Scope: the switches named by the orchestrator, plus the opt-ins on three pages:

- `r21-unsafe-field-arm-dark-under-byte-field-offsets-20261018.md`;
- `r21-vh-const-refuses-cast-and-statement-sites-20261019.md`;
- `r21-lean-allocation-rows-cost-is-outside-the-inline-sequence-20261018.md`.

The review was by reading only, at HEAD 98007436be: nothing was built or run. Line numbers are HEAD's.

The review looked for hazards that a probe battery is likely to miss:

- SATB and card-mark gates;
- collector differences (Generational / G1 / ZGC);
- deopt and replay;
- exception order and messages;
- JMM ordering on x86-TSO;
- redefinition and unload races;
- multi-loader identity;
- JVMTI;
- the memory rule that memo / fast-path intrinsics often lose with the JIT on.

## Verdict table

| # | Switch | Verdict | One-line reason | Probe rows | Depends on |
|---|---|---|---|---|---|
| 1 | `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE` | SAFE-TO-FLIP | IR splice only; the store is fenced by the same `is_volatile_store` path as an unspliced one; replay is covered by the committed-store walk; single-pass keeps its refusal | `R21CfChain` cf-thenapply-*, `R21SpliceRows`, `R21UpdaterRows` (it declares volatile fields), `R21Fz5SpliceRethrow1` | `CRATONVM_JIT_IR_VOLATILE_FIELDS` and `_SPLICE_COMMITTED_STORE` (both default ON); the CF rows also need the handler trio |
| 2 | `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` | SAFE-TO-FLIP | The TSO argument is sound (escape = every non-base use, CFG reachability without re-entering the allocation block); x86 only | `R21Fz9FreshVolatile1`, `R21CfChain`; w24: declines under a debugger / a C agent's `can_access_local_variables` | #1 for the spliced CF store |
| 3 | `CRATONVM_JIT_IR_ALLOC_PREFETCH` | SAFE-TO-FLIP | A prefetch writes nothing and never faults; the encodings are right; a perf-only risk past the TLAB end | `R21AllocLean` ladder and controls, `R21CfChain`, `R21SpliceRows`, w24 `R21FlipRows` mt-* (4 threads) | #20 for objarr16 on Gen / G1 |
| 4 | `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD` | SAFE-TO-FLIP | A null old value owes no pre-barrier on every collector that publishes a plan. The racing-writer window is HotSpot's own. The post gates still run | `R21CfChain` at `-Xmx64m`, `R21UpdaterRows`, `R21VhConstRefRows` (CAS / CAE with a null expected) | the gate plan (Gen, ZGC; G1 has no vh-const plan); applies inside #9 and #10 too |
| 5 | `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD` | FLIP-AFTER-FIX | `note_bytecode_copied` runs AFTER the class-manager guard is dropped, which breaks the documented "mark under the read guard" contract (redefinition race) | `R21CfChain` cf-complete / cf-*-done | none |
| 6 | `CRATONVM_JIT_IR_SELF_COPY_BY_ID` | SAFE-TO-FLIP | Comparing ids is stricter across loaders than comparing names. Residual: the resolution walk is `find_method_recursive`, a dispatch walk, not JVMS 5.4.3.3 | `R21SpliceRows`, `R21Fz2SpliceDeep1/2` | none |
| 7 | `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID` | SAFE-TO-FLIP (residual FIXED w24, pending build) | Correct while every splice is recorded on its body. A body whose copy record was lost (`try_borrow_mut` failed) but which holds other copies was under-withdrawn; w24 falls back to the name when the cache-wide copy set names the class; w25 (lane initwitness, pending build) also flags the body itself (`CompiledMethod::copy_record_lost`) | `R21FinalizeAncestorCtor`, `R21FinalizeCtorThrows`, `R21Fz3FinCtorChain1`, `R21Fz5FinTransitive1`; a two-loader row is missing | `CRATONVM_JIT_IR_SPLICE_*` record copies; `SCOPE_REDEFINITIONS_BY_COPIED_CLASSES` |
| 8 | `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` | SAFE-TO-FLIP | Fail-closed in every interleaving. Perf nit (FIXED w24, pending build): a stale row whose re-ask now ALLOWS was never cleared, so it was re-asked on every visit | `R21FinalizeRetryInit`, `R21Fz9FinLater1`; a redefinition row is missing | none |
| 9 | `CRATONVM_JIT_VH_CONST_CAST_CAE` | SAFE-TO-FLIP | One read decides both the cast and the outcome. A failed cast or a lost race writes nothing, so the call exchanges and then throws as HotSpot. A failed CAE is a plain-MOV getVolatile, valid on TSO | `R21VhConstRefRows` cae-cast-* (cae-cast-wrong-hit), `R21Fz9VhCaeCast1` | #4 (optional) |
| 10 | `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE` | SAFE-TO-FLIP | VM files only `arrayType == Object[]`, not invoke-exact; store check by own component id / display; every refusal before the write keeps NPE / AIOOBE / ASE order; no Field null-old for arrays | `R21VhConstRefRows` oarr / sarr / narr / csarr / nested / *-oob / *-both-bad / null-*, `R21Fz9VhArrRef1` | `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM` (ON) for subtype values; #4 for CAS / CAE under marking |
| 11 | `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO` | SAFE-TO-FLIP | The memo answer is the scan's answer at every return pc. It depends on the bytes only, so sharing it through the intern table is correct | `R21Fz8FinPrologue1`, `R21FinalizeEmptyOverride`, `R21Fz6EmptyFinalize1`, `R21ReflectCtorRegistration` | `finalizer_register_at_object_init` |
| 12 | `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER` | SAFE-TO-FLIP | The prefilter rules out only on (same id AND a different hash). Its word follows the recorded name wherever the rule reads it; ids are never reused; the word is cleared at unload | `R21Fz5HiddenOsr1`, `R21Fz7HiddenSelfLambda1`, `R21Fz8HiddenDeoptStorm1`, `R21Fz8HiddenNestmate1`, `R21HiddenLookupIn` | `CRATONVM_HIDDEN_SELF_CP_BY_ID`, `_RECORDED_NAME` (ON) |
| 13 | `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO` | KEEP-OFF (fix first); defect FIXED in wave 24 (lane names, pending build), flip after the concurrent-mark row | Hands out a raw `String` pointer held only by the weak intern table and a non-root TLS line, with no keep-alive while marking (array / primitive mirrors). Also a no-JIT-only candidate | `R21FzReflName1`, `R21Fz2ReflNameReload1`, `R21ReflectInvoke`; a concurrent-mark row is missing | `CRATONVM_CLASS_GET_NAME_MEMO` (ON) |
| 14 | `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS` | SAFE-TO-FLIP | The decode admits exactly `16 + 8*slot`; arrays (real byte offsets that look identical) are excluded by `GC_FLAG_COMPACT`, which arrays never carry; statics and minted offsets sit in the synthetic band | `R21UpdaterRows` ifu / lfu / rfu-cas, `Nio21CasRace` bench, `R21FzUpdUnsafe1`, `R21Fz3UpdaterOdd1`, `R21Fz5VhUnsafeMix1`; w24 unit test: no array header carries `GC_FLAG_COMPACT` | `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (ON), `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE` (ON); flip with #15 |
| 15 | `CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL` | SAFE-TO-FLIP | Publishes only from the compact arm, settles only lifetime / epoch refusals; one relaxed load when off | same as #14 | inert without #14 under byte offsets |
| 16 | `CRATONVM_JIT_VH_CONST_CAST_RETURN` | SAFE-TO-FLIP | Pure load + post-load screen; decline re-reads; class id only when uniquely resolved for the caller | `R21UpdaterRows` vh-ref-cast-get / vh-ref-getacquire / vh-cast-get-*, `R21CfChain` vh-get-control | none |
| 17 | `CRATONVM_JIT_VH_CONST_VOID_RMW` | SAFE-TO-FLIP | A `V` descriptor only drops the result; invoke-exact handles are never filed (they would throw WMTE) | `R21UpdaterRows` vh-int-getandadd-void / vh-int-cas-void | none |
| 18 | `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET` | SAFE-TO-FLIP | `Object[]`-typed handles only; ref-array kind byte, unsigned index; ZGC read barrier and narrow oops refused (IR parent and SP parent both check) | `R21UpdaterRows` vh-oarr-getacquire, `R21VhConstRefRows`, `R21Fz9VhArrRef1` | none |
| 19 | `CRATONVM_JIT_VH_CONST_CAST_RMW` | SAFE-TO-FLIP | The cast is decided before the swap by an exact-value `LOCK CMPXCHG`, so a failed cast writes nothing and the call swaps and then throws | `R21UpdaterRows` vh-ref-getandset | none |
| 20 | `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC` | SAFE-TO-FLIP | A zeroed body is all-null. A TLAB is young / eden on Gen and G1. Header before commit, as for the primitive arm that is already ON there. The concurrent-cycle start moves to the refill, as for primitive arrays | `R21AllocLean` lean-static-objarr16 with `-XX:+UseGenerationalGC` / `-XX:+UseG1GC`, `R21Fz2AnewArr1`, `R21BarrierRows`, w24 `R21FlipRows` mt-objarr16 | none |
| 21 | `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD` | SAFE-TO-FLIP | The length of a fresh constant-length array is exact; negative and over-`i32` lengths are skipped; exception anchors are skipped | `R21AllocLean` local-byte16, `R21Fz2ArrLen1`, `R21EaRows`, w24 `R21FlipRows` deopt-* | flip with #22 for the row |
| 22 | `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM` | SAFE-TO-FLIP | Primitive only, at most 64 elements, every use `Dead`. A frame-state reference is a `Guard` input, so it is not `Dead` and refuses the array | `R21AllocLean` local-byte16, `R21Fz2ArrElim1`, `R21Fz4EaMaterialize1`, `R21EaRows`, w24 `R21FlipRows` deopt-* / dead-* / dead-negative | #21 |
| 23 | `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE` (on the page; already default ON) | n/a | kill switch of #14 / #15 only | | |
| 24 | `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM` (on the page; default ON) | n/a | `=0` only narrows #10 / #16 | | |

(Rows 23 / 24 are cross-references, not separate verdicts.)

## Per-switch notes

### 1. `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE` -- SAFE-TO-FLIP

Code:

- the reader is `jit/src/ir.rs:2396` (it requires `IR_LOWER_FENCES_VOLATILE_STORES` and `ir_volatile_fields_enabled`);
- the resolver admission is `vm/src/runtime/interpreter/jit_bridge.rs:34014`;
- the rebase is `jit/src/lib.rs:17795`.

Checked:

- **The single-pass tier keeps its refusal.** The admission is `!(ir_mode && ..)`, and no resolution cache is shared
  between the tiers. The comment "recorded in BOTH modes" is only about the load pcs.
- **The fence.** The store is the builder's ordered `Op::Store`, and `ir_lower.rs:26489` emits `LOCK ADD` after every
  `is_volatile_store()`. A spliced store and an unspliced one take the same path.
- **Replay.** `CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE` (default ON, read at `lib.rs:60250`) refuses a later trap
  inside the splice. A deopt therefore never re-executes the store. A re-executed volatile store would be a visible
  lost update, so this is the load-bearing condition.
- **Barriers.** A reference volatile store takes the ordinary IR store barrier arms.

Verify: `R21UpdaterRows` under the switch alone (arm C on the CF page). Its volatile-field helpers may now splice into
callers.

### 2. `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED` -- SAFE-TO-FLIP

Code: `jit/src/ir_lower.rs:912-1060` and `26276-26300`.

**The argument holds on x86-TSO.** The store buffer drains FIFO, so any thread that sees the later publication also
sees the store. A Dekker witness needs the object, so the other thread cannot observe the missing StoreLoad. The
escape rule is conservative:

- the base must be directly `Op::New`;
- every use other than `inputs[2]` of a `Load` / `Store` counts as an escape, phis included;
- an escape that has no schedule position refuses the store;
- the reachability walk is cut only at the allocation's own block.

Deopt between the store and the publication is harmless: TSO ordering does not depend on the compiled code
continuing.

The page's "known limit" (a JVMTI agent publishing the object from a local) is narrower than stated. The agent must
see the object BEFORE the store, which needs `can_access_local_variables` and a safepoint inside the window.

- Optional hardening: compute the set as empty while JVMTI `can_access_local_variables` is possessed. This is
  HotSpot's EA opt-out.
- The aarch64 backend does not read the set, so the switch is x86-only by construction. Keep it that way: on ARM the
  publication store can pass the volatile store.

**Wave 24 (lane flipprep): the optional hardening is DONE, pending build.** Per VM and per compile, no process
global, default behaviour unchanged:

- new `CompileRequest::jvmti_locals_accessible`. The VM's door fills it only while the switch is on, from
  `jvmti::native_env::locals_capability_may_be_held`. That reads this VM's `onload_acquired`
  `can_access_local_variables` bit. The capability is OnLoad-only, so a live env can hold it only if a startup env
  acquired it, which makes the bit a superset of "possessed now". It uses `try_lock`: a held JVMTI lock answers
  `true` (fail-closed) instead of deadlocking;
- the IR door enters `ir_lower::CompileLocalsObservedScope` with `debugger_observes_locals ||
  jvmti_locals_accessible`, and `Lowerer::volatile_store_fence_elided` computes the set empty inside it. JDWP
  (`debugger_observes_locals`) counts too, because a JDWP agent holds the capability on HotSpot;
- tests: `native_env` `flipprep_locals_capability_may_be_held_is_per_vm_and_fail_closed`, and `ir_lower`
  `r21w24_flipprep_locals_observed_scope_nests_and_gates_the_elision` (scope nesting plus a source pin of the gate).

### 3. `CRATONVM_JIT_IR_ALLOC_PREFETCH` -- SAFE-TO-FLIP

Code: `jit/src/runtime_lowering.rs:560-650`. The emission is after the end check in both bumps (`:950`, `:1440`).

- The encodings are correct: `0F 0D /1` with modrm `0x88` (PREFETCHW [RAX+disp32]), and `0F 18 /0` with modrm
  `0x80` (PREFETCHNTA).
- RAX is the new top and is not written. Flags are not touched.
- No slow-path jump or patch site depends on the length of the bump, because `emit_jcc` patches are positional.

Perf caveats, not correctness:

- `PREFETCHW` past the TLAB end can steal a neighbour thread's line in exclusive state;
- on ZGC, a prefetch into reserved but uncommitted arena space costs a page walk.

The A/B must include a multi-threaded allocation row (`R21CfChain` with several threads, or the Spring Boot sample)
before the flip.

**Wave 24 (lane flipprep): the row exists, not yet run.** It is `tools/probes/jit/r21/R21FlipRows.java`, rows
`mt-byte16` / `mt-objarr16` / `mt-fields`. Four threads allocate in tight loops and check that every fresh
allocation is zeroed. Each stamps its own mark on what it allocates and keeps a 256-entry ring. After the join, the
whole ring is checked again (`bad=0 ring=130560`). The timing lines are `time mt-<kind>-t4`: wall ns per allocation
over all threads. Run it with the switch on and off, on every collector, and with #20 on Gen / G1.

### 4. `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD` -- SAFE-TO-FLIP

Code: `jit/src/ir_lower.rs:74270-74400`, and `74881` (array CAS / CAE).

- **ZGC.** The pre byte is a per-heap byte armed only by `set_mark_active` (`gc/src/zgc.rs:7509`).
- **Gen and G1.** The byte is armed by marker counting (`gc/src/gen_heap.rs:64860-64950`).
- **Nothing else arms it** except `clear_jit_ref_store_plan`. That is the legacy VM-less teardown. It arms
  pre / post as a kill-all, and under this switch a null-old store whose receiver the baked mask calls young falls
  through it. No production caller was found (only tests), so this is a note, not a defect.
- **`Expected`** is exact: a CAS overwrites `expected` or nothing.
- **`Field(disp)`** reads the receiver's compact field before the store. A racing write in that window is overwritten
  unlogged. This is the same window HotSpot's G1 pre-barrier has, and SATB covers it (the racer's value was reachable
  at the snapshot or allocated during the mark).
- **getAndSet / getAndBitwise** keep the decline.
- **Arrays** never get `Field`: the index is variable (`74877`).
- **The precedent** is the g1put `emit_g1_ir_ref_putfield` null-old test (`ir_lower.rs:20999`), which is default ON.

The probe must run at `-Xmx64m` on Gen and on ZGC, so that marks actually run (the CF page has the run lines).

### 5. `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD` -- FLIP-AFTER-FIX

**Defect.** In `vm/src/runtime/interpreter/jit_bridge.rs` `call_entry_null_guard_answer`, the function reads the
target's bytecode and decides the guard field (`entry_null_guard_field(code...)`) inside a block that holds
`try_read_untracked()`, at about 18690-18742. That block ends before `breakpoint_bars_compiling`,
`jit_resolve_field_site` and `class_layout`. Then, at `:18773`:

```rust
shared.jit.jit_cache.note_bytecode_copied(declaring_id);
```

This runs with no guard held. `JitCache::note_bytecode_copied` (`jit/src/lib.rs:40958`) says: "Call BEFORE reading
that bytecode, under the VM's class-manager read guard". The splice resolver does exactly that (`jit_bridge.rs:33821`,
before `drop(cm)`). Here the following interleaving is possible:

1. the guard drops;
2. a redefinition of the target class takes the writer;
3. `JitRealm::scoped_redefinition_admitted` finds no mark, and the body is not withdrawn;
4. the compiled caller keeps skipping a call whose NEW body may do work while the field is null.

The open compile's `BytecodeReadWitness` probably refuses the `put` anyway, so the window is narrow. But the
contract is the one the redefinition path relies on.

**Fix.** Move `note_bytecode_copied(declaring_id)` into the guarded block, right after `entry_null_guard_field`
answers `Some`. A later refusal still leaves a harmless extra mark.

Otherwise checked:

- the receiver null check declines to the call's NPE;
- legacy instances decline;
- interfaces, static, native, abstract, synchronized and `<init>` methods are refused;
- a native shadow keeps the call (bytecode authority);
- interp-only JVMTI events and scoped method / field watches bar the compile;
- the volatile field is read with a plain load (acquire on TSO);
- the layout epoch is guarded;
- the abstract interpreter admits no throwing, storing or calling instruction.

A StackOverflowError at the skipped call is lost, as with any inlining (C2 does the same).

### 6. `CRATONVM_JIT_IR_SELF_COPY_BY_ID` -- SAFE-TO-FLIP

Code: `jit/src/lib.rs:12645`; `jit_bridge.rs:33394-33420` and `33496`.

Comparing resolved declaring-class ids is loader-correct, where the name compare was not.

**Residual (exotic).** `resolved_class_id` comes from `classloading::find_method_recursive` (`class.rs:3489`). That
walk prefers a non-abstract method and falls back to interface defaults. It is a DISPATCH walk, not JVMS 5.4.3.3.
Two bytecode shapes go wrong:

- `Sub` declares an abstract INSTANCE `f()V` and `C` (its super) a static `f()V`. JVMS resolves to `Sub.f`, so
  `invokestatic` throws `IncompatibleClassChangeError`. The walk returns `C.f`, `is_static()` holds, and the copy is
  admitted as a self-call.
- A static interface method is not inherited, so it must give `NoSuchMethodError`.

Both shapes need hand-written bytecode or separate compilation. Follow-up: use the
`runtime::resolve::selection::resolve_declaring` facade (as #5 does) and require the resolved method to be static.
This is not a flip blocker; the pre-existing never-run exit shares the walk.

### 7. `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID` -- SAFE-TO-FLIP (residual)

Code: `jit/src/lib.rs:39895-39940`.

The by-id branch drops the `inlined_methods` name evidence whenever the body's `copied_classes` is non-empty.

**Residual.** `note_copy_in_open_compile` (`lib.rs:39609`) returns `false` on `try_borrow_mut` failure. The copy then
goes only to the cache-wide `copied_bytecode_classes` set and not onto the body. Take a body that:

- holds other recorded copies, and
- has a single-pass `<init>` splice of the class recorded only in `inlined_methods`.

The by-id rule does not withdraw it. The old rule would have. An un-withdrawn constructor body for a newly
finalizable subtype can miss a finalizer registration (a Java-visible difference). Cheap hardening: in the by-id
branch, also match by name when `JitCache::bytecode_was_copied(class_id)` (the cache-wide set) is true.

Also run the two-loader row the switch exists for: two loaders defining `p/Base`, only one finalizable subtype.

**Wave 24 (lane flipprep): the residual is FIXED, pending build.** Under the switch, the by-id branch of
`withdraws_for_finalizable_subtype` also accepts the `inlined_methods` name evidence when `copy_unrecorded` is set.
Before, it accepted that evidence only when the body's `copied_classes` was empty. `copy_unrecorded` comes from
`finalizer_copy_unrecorded_on_body`, which is `JitCache::bytecode_was_copied(class_id)` on the cache-wide set. It
is asked at two points:

- once at `invalidate_for_finalizable_subtype`, before the walk;
- at publication, through the new `InvalidationRecord::withdraws_in(cache, ..)` that `dependencies_current_since`
  now calls. Every other record kind delegates to `withdraws` unchanged.

The change only widens the withdrawal. Once our id is in the set, another loader's same-named splice can go too, in
the safe direction. Off, and for an id of 0, nothing changes. Test: `lib.rs`
`by_id_a_lost_copy_record_falls_back_to_the_name_through_the_cache_wide_set`. The two-loader probe row is still
missing.

**Wave 25 (lane initwitness, pending orchestrator build): the source side.** Wave 24 fixed the consumer. Now a failed
borrow in `note_copy_in_open_compile` (`jit/src/lib.rs`) also takes the conservative answer for the body itself:

- It still returns `false`, so the caller (`JitCache::note_bytecode_copied`) puts the copy in the cache-wide set.
  That is the conservative answer for every whole-cache path.
- It also sets a `Cell` flag beside the scope's `RefCell` (`CompileScopeSlot::copy_lost`). The `RefCell` is borrowed
  at that moment, which is why the record was lost; a `Cell` can still be written. The slot is still the one
  thread-local `COMPILE_CP_STAMP`, reached through `Deref`, so the statics ratchet does not move.
- Every body that compilation builds from then on carries `CompiledMethod::copy_record_lost`. Under
  `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID`, `withdraws_for_finalizable_subtype` then takes the body's `inlined_methods`
  name evidence (the old rule's answer), whatever the cache-wide set says.
- The outermost witness clears the flag when it opens the copied list and again when it closes it.

Only the opt-in by-id rule reads the flag, and it only widens that rule's withdrawal, so no new switch. Test: `lib.rs`
`by_id_a_body_whose_compile_lost_a_copy_record_is_judged_by_name`.

Not covered: unload withdrawal by copied class (`CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED`) reads only the body's
`copied_classes`, not the cache-wide set or the new flag. So an IR-tier copy whose record was lost is not withdrawn
when that class unloads. That is outside this switch, FILE. The two-loader probe row is still missing.

### 8. `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` -- SAFE-TO-FLIP

Code: `vm/src/runtime/interpreter/exception_dispatch.rs:940-975`.

Every interleaving with an epoch bump is fail-closed:

- a refused re-ask re-stamps the row;
- an allowed answer is never memoized here.

**Perf nit.** When the stale row's re-ask now ALLOWS, the row is left stale, so every later visit re-asks
`finalizer_bars_compiling`. The `jit_gate_pass` memo probably hides this. Even so, remove the descriptor (or re-stamp
the row) on an allowed re-ask.

Missing row: redefine a class whose constructor throws before `super()` into one that cannot, then check that it
compiles. The `--nojit` answer must be unchanged.

**Wave 24 (lane flipprep): the perf nit is FIXED, pending build.** When the read finds a row of another epoch that
names the descriptor, and the re-ask then allows, `finalizer_bars_compiling_memoized` takes the write lock once. It
drops that descriptor from the row, and drops the row when no descriptor is left. A row that another thread
restamped meanwhile (its epoch is no longer older than the current one) is left alone. Off the switch, rows are
never stale, so nothing changes. The wave-18 unit test now also checks that the stale row is gone under the switch
and kept off it. The redefinition probe row is still missing.

### 9. `CRATONVM_JIT_VH_CONST_CAST_CAE` -- SAFE-TO-FLIP

Code: `jit/src/ir_lower.rs:74793-74830`.

- `CMP RCX, R10` then the `MOV`s keep the flags.
- `LOCK CMPXCHG` with RAX = O replaces exactly O.
- The fail path screens the witness and writes nothing.
- When the cast fails, the call exchanges and then throws, as HotSpot does (`cae-cast-wrong-hit`).
- Reading a failed CAE with a plain MOV is a valid getVolatile on TSO. It relies on every volatile store carrying its
  trailing fence. The only exception is #2, whose argument covers this case.

`vh_const_class_by_name` answers only for the unique loaded class the caller resolves. A same-named class of another
loader fails the id or display screen and takes the call, which throws the `ClassCastException`.

### 10. `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE` -- SAFE-TO-FLIP

Code:

- `vm/src/jit/helpers.rs:25027-25075`: files only a real, non-exact `VarHandleReferences$Array` whose `arrayType` is
  exactly `[Ljava/lang/Object;`;
- `jit/src/ir_lower.rs:74855-75000`;
- the IR emitter `14263`; the SP emitter `x64/op_invoke.rs:13572`.

The screen matches the JDK body:

- **The cast.** `arrayType.cast` admits every reference array, and the screen admits every reference array.
- **The store check.** `runtimeTypeCheck` admits exactly what the screen admits: null, an `Object[]` (component id 0),
  the exact component, or a display subtype or implementor. An array value declines.
- **Exception order.** Every refusal comes before the write, so `set` keeps ASE-before-AIOOBE and
  `setVolatile` / CAS keep AIOOBE-first, from the JDK body.

Other checks:

- **Barriers.** The gates come from the published plan, and G1 has none, so nothing is served there.
- **Collectors.** The IR and SP parents check for an armed ZGC read barrier, and narrow oops refuse.
- **No safepoint between the halves.** Nothing between the two halves can safepoint, so neither a promotion nor a
  mark start can intervene. The take-over freeze is covered by `scan_frozen_held_objects` (zr1br).

**Review point (no defect found).** Both tiers reload R11 / R10 / RDX between the halves and rely on that reload not
touching RAX. This holds for frame and register operands. A future `gp_load_value` form that materialises through RAX
would corrupt the array register silently. Consider an assertion in the emitter.

### 11. `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO` -- SAFE-TO-FLIP

Code: `exception_dispatch.rs:442-520`; the cell is in `reader/src/quickened.rs:136`.

- `ctor_facts_answer` gives "the first `invokespecial` pc < return_pc", which is the linear-order predicate of
  `ctor_body_keeps_receiver_and_calls_before` (`:530`).
- The `Arc::ptr_eq` check ties the cell to the frame's own bytes.
- The racing first fill is idempotent.
- The scan reads no configuration, so sharing the cell between VMs is correct and is not compatibility state.
- The hook is interpreter-side and does no work beyond the memo, so it needs no `!jit_enabled()` gate.

### 12. `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER` -- SAFE-TO-FLIP

Code:

- `classloading/src/class_manager.rs:4398-4460` (the reader and `self_name_rules_out`);
- the record sites `12518`, `13433` and `14162`;
- `vm/src/jit/helpers.rs:37544`.

**Consistency.** When a recorded name exists, the rule (`hidden_self_name_rule`, `:4561`) accepts exactly that one
name, and the prefilter hashes exactly that name:

- the define path (`options.hidden`) records `this_class` in both places;
- `mark_class_hidden` records the prefilter word only when a recorded name exists;
- a class with no recorded name (the stored-suffix fallback) has no word, so the prefilter never rules it out.

Unload clears the word, and ids are never reused. Release / Acquire ordering is kept, and the word is written before
publication.

**Constraint for later changes.** If the self-name rule ever accepts a SECOND spelling (for example the stored
`/0x..` name next to a recorded one), the prefilter must hash that spelling too. Otherwise it becomes a
false-negative self test, which is exactly the `R21Fz5HiddenOsr1` IllegalAccessError. Note it at
`hidden_self_name_rule`.

### 13. `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO` -- KEEP-OFF until fixed

Code: `native-builtins/src/lang_class.rs:2998-3150`.

**Defect.** The memo is a non-root TLS line that holds a RAW address of the answer `String`. For mirrors with no
class id (array mirrors the reverse map does not hold, and primitives: the very rows this memo was added for, see
`r21-class-get-name-memo-misses-array-and-primitive-mirrors-20261018.md`), the noting path does not hold the String
strongly:

- `native_class_get_name_memoizing`, `:3044`, notes after `class_get_name_memo_fill` returned early with no class id,
  so no global-root row exists;
- the mirror's `name` slot holds the INTERNAL name, not the answer (`:2936`);
- what remains is the interned String, and `vm/src/memory/string_table.rs` holds interned Strings WEAKLY.

So a hit can hand Java a String that is only weakly reachable. During a concurrent mark (Generational old mark, G1,
ZGC) it does so without the keep-alive (SATB enqueue) that a weak-table load owes:

1. The String was unreachable at the snapshot, so it is not marked.
2. The mutator stores it into the heap. The pre-barrier logs the OLD value, not this one.
3. The String is reclaimed at the cycle's end.
4. The heap reference now dangles.

The address epoch (`HeapRealm::address_epoch`) only invalidates the TLS line. It does nothing for references Java
already stored. This is the weak-resurrection bug class, not an epoch question.

`REAL_HANDLE_MEMO`, whose soundness argument this memo borrows, memoizes a Rust record and never re-hands a heap
pointer, so the argument does not carry over.

**Fix.** Either change is enough:

- (a) note a mirror line only when the answer is held strongly: a class-id row exists, or `class_get_name_memo_fill`
  succeeded. This keeps the class-id rows and drops the array / primitive gain;
- (b) on a hit while any SATB pre gate is armed, decline (or enqueue the answer as the intern lookup does).

**JIT rule.** This is a leaf-native memo on `Class.getName()`. With the JIT on, the compiled call path pays the thin
call plus a TLS lookup, against HotSpot's field load. A/B both `--nojit` and default; if the JIT arm loses, gate the
memo on `!jit_enabled()`.

Missing row: a loop of `int[].class.getName()` / `int.class.getName()` stored into a list, then cleared, under a
concurrent mark (`-Xmx64m`, Gen and G1). Then `System.gc()` and compare every stored name.

**Wave 24 (lane names): defect FIXED in code, pending orchestrator build; the switch stays opt-in.** Same switch,
no new one. Fix (a), keeping the array / primitive gain:

- a line is noted only for an answer something holds STRONGLY. `class_get_name_memo_fill` now answers whether it
  filed a class-id row (its global root holds the answer); a class-id memo hit notes from that row;
- a mirror with no class id gets a per-VM strong hold instead: `ReflectDoorCaches::class_names_idless`, answer text
  -> a global root of the interned answer (`class_get_name_hold_idless_answer`). That is what HotSpot's `Class.name`
  field does for every mirror. One row per distinct text, never removed before the VM goes; a row can outlive an
  unloaded array class's mirror (memory only). Another object already held for the same text declines the note;
- so a hit hands out a String a root already traces, and needs no keep-alive in any phase of a cycle (option (b)
  alone would still leave G1's remark-to-cleanup window, where the mark is over but the String is not yet freed).

**JIT rule: not gated on `!jit_enabled()`.** With the JIT on, `Class.getName()` is still the same leaf `Intrinsic`
(the JDK body cannot run: `Class.name` holds the internal name here). The line replaces the class-id hit's three
locks on the same compiled door; it does not compete with compiled bytecode. The A/B in both arms is still owed.

Test: `lang_class` `tests::r21w24_mirror_memo_notes_only_strongly_held_idless_answers`. The concurrent-mark row
above is still missing and is the flip gate.

### 14 / 15. `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS` + `_VOLATILE_PUBLISHES_FIELD_CELL` -- SAFE-TO-FLIP, together

Code:

- `jit/src/direct_helpers.rs:2877-2900` and `3272-3340`;
- `vm/src/vm/vm_exec.rs:70055-70120` (`note_unsafe_volatile_field_cell`) and `11178-11300` (`hw_atomic_addr_checked`).

**The decode.** `SUB 16 ; TEST low3 ; SHR 3` maps an offset below 16, the synthetic band and the `Buffer.address`
sentinel all to slots above `0x7FFF`, which decline.

**Arrays are the load-bearing case.** Unsafe array offsets are REAL byte offsets (`16 + 8*i` for `long[]` and
`Object[]`), so after the decode they look like a slot `i`. An array header's class word is its COMPONENT class id
(for example `ConcurrentHashMap.Node[]` carries `Node`'s id), so the cell key alone would alias a `Node` field cell.
Arrays are excluded only because `GC_FLAG_COMPACT` is never set on an array (`types/src/heap_types.rs:824-830`) and
the screen tests it first.

Keep that invariant. Add a `const` assertion or a test that the array allocators never set `GC_FLAG_COMPACT`. A future
"compact array header" would silently make CHM `tabAt` read a field cell.

**Wave 24 (lane flipprep): the test is ADDED, pending build.** It is `direct_helpers`
`r21_flipprep_no_array_header_carries_gc_flag_compact`. For every `ArrayElementType` it checks the byte the
screen's `TEST BYTE [RAX + GC_FLAGS]` reads, in three places:

- `ObjectHeader::new(.., ObjectKind::Array, ..)`, the mark word both inline array allocators (SP and IR) take;
- that header after the mark / old-gen flag transitions;
- `cratonvm_gc::Heap::alloc_array`.

No production code changed.

Publication only marks cells. Its refusals settle only for lifetime / epoch reasons. Flip both together: #15 is inert
without #14 under byte offsets.

### 16-19. `CRATONVM_JIT_VH_CONST_CAST_RETURN`, `_VOID_RMW`, `_REF_ARRAY_GET`, `_CAST_RMW` -- SAFE-TO-FLIP

Code: `ir_lower.rs:73380-73545` (`vh_const_core_cast_return`, `vh_const_result_screen`); `74540-74585`; `74764-74790`.

- The result screens are null-passing, plain-object-only (an array value declines unless `NullOnly`), and exact or
  display / interface.
- `VOID_RMW` and the `V` CAS shape occur only in hand-written bytecode for `compareAndSet` (JLS 15.12.3 keeps `Z`).
  For `getAndAdd` javac emits it. An exact-invoke handle is never filed (`helpers.rs` field plan `exact` check), so
  no `WrongMethodTypeException` is skipped.
- `CAST_RMW` uses the same one-read, exact-CMPXCHG discipline as #9.
- `REF_ARRAY_GET` has no load barrier, and is refused under an armed ZGC read barrier in both tiers' parents
  (`ir_lower.rs:13849`, `op_invoke.rs:13295`).

### 20. `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC` -- SAFE-TO-FLIP

Code: `jit/src/runtime_lowering.rs:552-558`, `1310-1325`; `ir_lower.rs:34770-34780`.

- The ZGC arm is unchanged: it still needs the announce.
- On Gen and G1 the reference arm takes the primitive arm's header-before-commit sequence, which is already default
  ON there.
- An all-null body needs no barrier, and a TLAB is young (eden) on both collectors.

Two behaviour changes go with it, as for primitive arrays:

- the concurrent-cycle start (`jit_drive_g1_concurrent_mark`, `jit_take_gen_inline_concurrent_start`) and the SATB
  flush in `jit_anewarray_object` now run once per TLAB refill instead of once per allocation;
- `note_anewarray` census counts fall.

Watch the G1 / Gen GC-start cadence on an `Object[]`-only loop (`CRATONVM_GC_STATS=1 --verbose:gc`). The concern is
that the old-gen mark starts later, not a correctness hazard.

### 21 / 22. `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD` + `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM` -- SAFE-TO-FLIP

**The fold** (`jit/src/ir_optimize.rs:3221-3285`):

- it rewrites in place, and only when `0 <= n <= i32::MAX`;
- it skips an `ArrayLength` that anchors an exception edge;
- a `newarray` that throws OOM or NASE still runs, because only the length use is rewritten.

**The elimination** (`jit/src/escape_analysis.rs:2490-2510`):

- primitive arrays only, constant length at most 64, every use `Dead`;
- a deopt frame state references the array as a `Guard` input, which is not `Dead`, so a deopt-live array is refused.

Elimination drops a possible OOM for arrays of 64 or fewer elements, as C2 does (`EliminateAllocationArraySizeLimit`).

Verify: one deopt row with a 9..64-element unused array live across a guard. It should keep the allocation, which
tests the `Guard`-use claim.

**Wave 24 (lane flipprep): the rows exist, not yet run.** They are in `tools/probes/jit/r21/R21FlipRows.java`:

- `deopt-int32` / `byte9` / `long64` / `double17` / `char48` / `bool64`. The only hot-path use of the array is its
  length, which the fold targets. A rare arm that initialises a fresh `LateN` class is the deopt point, and the
  interpreter writes into and sums the array there;
- controls `deopt-short8` and `deopt-int65`;
- `dead-*64` for every primitive type;
- `dead-negative`: a dead `new int[-1]`, whose constant length is "at most 64" but must still throw
  `NegativeArraySizeException` with message `-1`;
- `time dead-int64` / `dead-long64`.

The expected lines, written by reading, are in the file header. Also run with `CRATONVM_DBG_SCALAR_DEOPT=1`: no
`MATERIALIZE` may appear for a deopt row's array.

## Cross-cutting

- **Pairs to flip together:**
  - #14 with #15;
  - #21 with #22;
  - #1 with #2 (the CF row);
  - #3 with #20 on Gen / G1.
- **#4 applies inside #9, #10 and #19.** Run the SATB rows with them on.
- **No-JIT-only candidates:**
  - #13, the only memo-in-a-native;
  - #11 and #12 are not fast-path intrinsics: #11 is the interpreter hook, #12 the compiled-route helper.
- **Missing probe rows, to write before the flip:**
  - (a) #13: weak-intern names under a concurrent mark;
  - (b) #5: a redefinition race on the null-guard target, retransforming `postComplete`-like bodies while compiled
    callers run;
  - (c) #7: two loaders with the same-named finalizable ancestor;
  - (d) #8: redefinition that makes a barred constructor compilable;
  - (e) #22: a deopt with an unused 9..64-element array (w24: written as `R21FlipRows` deopt-* / dead-*, not yet
    run);
  - (g) #3 / #20: a multi-threaded allocation row (w24: written as `R21FlipRows` mt-*, not yet run);
  - (f) #4: SATB at `-Xmx64m`, Gen and ZGC, with `R21VhConstRefRows` too.
