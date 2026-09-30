# Call-argument copies are still rooted for the whole call on every call shape but one

> **STATUS (2026-09-29, gce ve2): OPEN -- f-7 attribution is inverted: argspecial passes because of the single-pass local-map claim (CRATONVM_GC_SP_LOCAL_MAP_ROOTS; _noloc fails 3/3 on every collector), not the deopt residue claim (_nores fails only Generational private-special). Row 8: deadspill=1422 on both base and e2, no increase.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/f): OPEN -- the `argspecial` gain is
> attributed by reading, with the rows that confirm it; rows 1-7 unchanged.**
>
> - **Attribution.** On e1 `Gcd1ArgPinSpecialProbe` under `$SP` went from
>   0/6 (Linux base) to 6/6 on all three collectors, and the
>   `CRATONVM_JIT_CALLEE_OWNED_ARGS=0` arm still fails 6/6, so the owned-args
>   site is necessary and something e1 added dropped the LAST holder. By
>   reading it is the frame-deopt register residue claim
>   (`CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS`, `conservative_roots.rs`
>   `frame_deopt_image_is_residue`): each callee calls
>   `new WeakReference<>(a)` (whose `Reference.<init>` frame holds `a` in its
>   locals and registers) and then `cleared(ref)`, whose frame occupies the
>   same stack depth and whose never-written deopt block overlays those words
>   while it runs `System.gc()`. The single-pass local claim cannot be it
>   (`cleared`'s non-parameter locals are zeroed by its prologue; `a` is
>   nulled in a named home). On Windows the base already passes 3/3 (a
>   different stack layout), so the rows must run on Linux.
> - **Attribution rows** (Linux, each collector, `$SP` as in `ve1.list`):
>   ```
>   p ${gc}_argspecial_nores_$r 120 "$SP CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0" "$X -Xmx64m" Gcd1ArgPinSpecialProbe
>   p ${gc}_argspecial_noloc_$r 120 "$SP CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0" "$X -Xmx64m" Gcd1ArgPinSpecialProbe
>   p gen_argspecial_nores_census 120 "$SP CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0 CRATONVM_DBG=oldmark-root-census,root-source" "-XX:+UseGenerationalGC -Xmx64m" Gcd1ArgPinSpecialProbe
>   ```
>   Expected if the reading holds: `_nores` FAIL (as the base), `_noloc`
>   = HS, and the census names `region=deopt-saved-gpr-image` in `cleared`
>   or `System.gc`'s caller.
> - **Row 8 run (from e1, not yet run):**
>   `p gen_row8_$r 300 "CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_DBG_JIT_ROOTSCAN=1" "-XX:+UseGenerationalGC -Xmx256m" R10SelfRecCatch`
>   (the probe is `regression-suite/probes/R10SelfRecCatch.java`; put that
>   directory on the row's classpath) on e1 vs base: stdout = the recorded
>   output, and the `[regoop] ...
>   deadspill=` count on stderr higher on e1.
> - **Rows 1-7:** unchanged; still design items (see below and
>   `gcd-d9d-proposal-no-call-site-rerun-from-entry-20260928.md`).

> **STATUS (2026-09-29, gce e1/x): KEEP -- rows 1, 2 (kinds 0/2) and 3-7 open; `Gcd1ArgPinSpecialProbe` now PASSES on e1.** `verify-e1/ve1`, `CRATONVM_JIT_IR_CALL=0 CRATONVM_JIT_IR_CALL_SPECIAL=0 CRATONVM_JIT_IR_CALL_VIRTUAL=0`, `-Xmx64m`: `*_argspecial_1..2` print HotSpot's `PASS all 3` on Generational, G1 and ZGC, 6/6 (base 0/6, `FAIL 3 of 3`); the kill-switch control `*_argspecial_own0_*` still fails 6/6; `*_argpin_sp_*` fail `arg-pin-warm-caller` 6/6 (row 5, by design). Which e1 change moved `argspecial` is not attributed; the candidates are e1/f's single-pass java-local claim and the owned-args hand-off verdict. **Remaining:** attribute it (the same rows with `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0`, then `CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT=0`); row 8's `[regoop] deadspill=` run under `CRATONVM_NO_MOVING_YOUNG=1` (not run); rows 1, 2 (kinds 0/2), 3-7 (the d9d proposal).

> **STATUS (2026-09-29, gce e1/f): NARROWED -- row 8 FIXED IN CODE; rows 1,
> 2 (kinds 0/2), 3-7 open, unchanged.**
>
> - **Row 8 (landed):** `jit/src/x64/safepoint.rs` `emit_safepoint_metadata_only`
>   now records `pending_live_frame_hi = next_spill_offset` on both arms (the
>   statement `emit_pre_safepoint_spill` makes), so a metadata-only self-call
>   safepoint under `CRATONVM_NO_MOVING_YOUNG` no longer ships
>   `live_frame_hi = 0` and the dead-spill claim drops the popped argument
>   slots above the cursor. No emitted byte changes; only the map's bound.
>   Test: `cargo test -j 5 -p cratonvm-jit --lib gce_e1f_a_metadata_only_safepoint_records_the_cursor_without_moving_young`.
> - **Related, landed on another page:** the single-pass java-local claim
>   (`gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`)
>   drops an unnamed local home at every call shape; it does not touch the
>   argument copies of rows 1-7, which the maps name on purpose.
> - **Rows 1-7:** unchanged; each needs a new post-call answer (see the rows),
>   and rows 4-5 must also satisfy d10/f's replay constraint. Row 5 is the
>   shape `Gcd1ArgPinProbe`'s warm call takes; its caller calls `make` first,
>   so no row-5 fix may cover it.
> - **Run:** row 8 has no probe of its own (it needs `CRATONVM_NO_MOVING_YOUNG=1`
>   and a self-recursive caller holding a dropped argument). Under
>   `CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`, the
>   `[regoop] ... deadspill=` count should grow against the base on a
>   self-recursive workload (`R10SelfRecCatch`), with its recorded output
>   unchanged, on each collector.

> **STATUS (2026-09-28, gcd d10/f, lane frames10): NARROWED -- row 2's
> statically bound half is fixed in code; rows 1, 2 (kinds 0/2), 3-8 stay
> open, all owned by a later wave (lane f's files except row 8).**
>
> **Landed (default on, kill switch `CRATONVM_JIT_CALLEE_OWNED_ARGS=0`, the
> same switch as d9/d's static rule):** `jit/src/x64/op_invoke.rs`,
> `walk_invoke_instance`'s plain direct arm, now applies d9/d's
> callee-owned-arguments rule: a single-pass direct instance call whose site
> is statically bound -- invoke kind 1: every `invokespecial` (a `super.m(..)`
> call, a constructor), and a private `invokevirtual` / `invokeinterface` the
> single-pass planner pins to its owner (JEP 181; `jit/src/lib.rs`, the
> `cp_invokespecial_owner_resolver` pin) -- into a body that owns its
> arguments (`crate::direct_callee_owns_its_arguments`) keeps no service
> copy: no range, no staged-oop naming, the argument registers out of the
> call's register mask and out of the spill image, and the owned-args
> service word (`emit_inline_callee_deopt_check_owned_args`). The rule:
> `sp_direct_call_args_owned_by_callee` now admits `(invokestatic, kind 3)`
> and `(invokevirtual|invokespecial|invokeinterface, kind 1)`, the latter
> only on an unguarded row (`guard_class_id == 0`: no intrinsic receiver
> speculation). Why kind 1 is safe exactly as kind 3: the callee-deopt
> service reads a receiver class out of the copy only for kinds 0/2
> (`vm/src/jit/helpers.rs` `jit_service_callee_deopt_body`,
> `receiver_class_id`); for kind 1 it resolves by name through the pinned
> owner (`callee_has_exception_table`, `jit_substituted_owner`), as for a
> static. Kinds 0/2 keep the copy. No retire cell or caller-held
> synchronized row reaches that arm. Tests: `op_invoke.rs`
> `gcd_d9d_owned_args_site_tests::a_statically_bound_instance_site_is_admitted_on_the_same_terms`
> (d9/d's two tests unchanged and still green by reading).
>
> **Second d10/f commit (the replay constraint, both arms):** a site is
> owned only where the caller's bytecode before the call, closed over loops,
> commits nothing (no store, invoke or monitor operation), so the rare
> hand-off's re-run of the caller is exact
> (`gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-20260928.md`).
> This withdraws ownership from every site whose caller called anything
> first, which is most of them, and every row below that could be extended
> is bound by the same constraint. Correction to row 5 and the ArgPin page:
> the IR tier is NOT off under moving-young (that gate was scoped away in
> `f78b72670`, 2026-07-31); by reading, `Gcd1ArgPinProbe`'s warm caller is
> an IR body by default (row 5), which is why d9/d's fix never moved that
> probe.
>
> **Still open, per row:** 1 (retire-cell sites: the fallback needs the
> range; emit it from the ABI registers first), 2 kinds 0/2 (the service
> needs the receiver's class: keep word 0 only), 3 (inline caches, the
> dispatch helper and the Rust doors' `CompileArgPins`), 4 (spliced direct
> calls: a hand-off from inside a splice leaves the ENCLOSING method, whose
> replay question is the splice's `spliced_bodies_side_effect_free`, so not
> a copy of the static rule), 5 (IR tier homes -- the shape
> `Gcd1ArgPinProbe`'s warm call takes by default; its site fails the replay
> constraint anyway), 6 (the interpreter's `JitArgPinGuard`: its `Stashed` /
> `Throw` re-runs have no caller frame to hand off to), 7 (aarch64), 8
> (`jit/src/x64/safepoint.rs`, JIT round 13's file). None was attempted
> without a build: each needs a new post-call answer, not a copy of the
> rule.
>
> **Run (each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`;
> the fix is in the JIT's frame layout, which every collector reads the same
> way):** `P="--java-home $JDK <gc-flag> -Xmx64m -cp tools/bench"`,
> `SP="CRATONVM_JIT_IR_CALL=0 CRATONVM_JIT_IR_CALL_SPECIAL=0 CRATONVM_JIT_IR_CALL_VIRTUAL=0"`
> (keeps call-carrying callers single-pass, so the rule under test is the one
> that runs)
> ```
> javac -d tools/bench tools/bench/Gcd1ArgPinProbe.java tools/bench/Gcd1ArgPinShapesProbe.java >   tools/bench/Gcd1ArgPinSpecialProbe.java tools/bench/GenR4W6JitOomRootProbe.java
> for i in 1 2 3; do env $SP timeout 300 cratonvm $P Gcd1ArgPinSpecialProbe; echo "rc=$?"; done
> env $SP CRATONVM_JIT_CALLEE_OWNED_ARGS=0 timeout 300 cratonvm $P Gcd1ArgPinSpecialProbe; echo "off rc=$?"
> timeout 300 cratonvm $P Gcd1ArgPinSpecialProbe; echo "default rc=$?"
> for i in 1 2 3; do timeout 300 cratonvm $P Gcd1ArgPinShapesProbe; echo "rc=$?"; done
> for i in 1 2 3; do timeout 300 cratonvm $P GenR4W6JitOomRootProbe; echo "rc=$?"; done
> cargo test -j 5 -p cratonvm-jit --lib gcd_d9d_owned_args
> ```
> Expected: `Gcd1ArgPinSpecialProbe` under `$SP`: `PASS super-special`,
> `PASS private-special`, `PASS static-direct`, `PASS all 3`, rc 0, 3/3
> (HotSpot `-XX:+UseSerialGC -Xmx64m` 3/3 and `-Xint` 1/1 print exactly
> that, checked 2026-09-28 with JDK 25.0.3); the `=0` arm is the control:
> `FAIL super-special`, `FAIL private-special`, `FAIL static-direct`,
> `FAIL 3 of 3`, rc 1 (by reading; a PASS there means the callee was
> spliced or the call not direct-bound, which makes that line vacuous). The
> default arm is reported, not gated: a line whose caller the IR tier
> superseded FAILs (row 5). `Gcd1ArgPinShapesProbe`: by reading
> `FAIL 4 of 4` now -- its `static-direct` caller calls `make` first, so the
> replay constraint refuses it, and rows 1, 3, 6 are open. `Gcd1ArgPinProbe`:
> see the ArgPin page (warm case FAIL by design). `GenR4W6JitOomRootProbe`:
> `PASS all 8`, rc 0, 3/3 on an idle host. `cargo test`: 6 passed (five
> site tests and d9/d's `gcd_d9d_owned_args_check_tests`).
> Retire row 2 kind 1 when the `$SP` loop passes on all three collectors;
> the page stays open for the other rows.

*Filed 2026-09-28 by gcd wave d9, lane d (args9), from the enumeration the
brief asked for (every path that keeps a copy of a call's reference
arguments in the CALLER after the call starts).*

- **Severity:** retention, MEDIUM, bounded by the call -- the same class as
  the parent page. What it costs is every structure a long-running callee
  received as an argument and dropped (a worker handed a batch it drops, a
  method that nulls its parameter and then allocates to an OOME or polls a
  `WeakReference`). HotSpot keeps none of them: outgoing arguments belong to
  the callee's frame, whose map drops a dead parameter.
- **Why each copy exists:** every one has a reader AFTER the call, on the
  callee-sentinel path: the re-run of a declined callee stash, the re-run of
  a frameless trap, and the run of the callee's own handler all read the
  ORIGINAL arguments (`vm/src/jit/helpers.rs`,
  `handle_compiled_callee_deopt_sentinel`). The copy is named in the map
  because a moving collection must rewrite it for those readers. The fixed
  shape shows the pattern: the copy can go where the callee's body makes the
  handler arm impossible and the frameless arm unexpected, and the remaining
  arm hands the trap to the caller's caller.

## The shapes, with the copy and its reader

| # | Shape | The copy | Its post-call reader | Why the d9/d rule does not cover it |
|---|---|---|---|---|
| 1 | single-pass direct `invokestatic` at a RETIRE-CELL site (every in-loop site under `CRATONVM_JIT_RETIRE_CELL`, default on; every site of an OSR body) | the service range (`reserve_direct_call_service_slots`, named via `pending_staged_arg_oops`) | the cell's dispatch fallback takes it as its argument buffer (`emit_retire_cell_call_target`), then the service | the fallback needs the arguments BEFORE the callee runs, so the range must be written; it could stop being named once the CALL has happened (a second, post-call map without it), which the single-pass map model cannot express today |
| 2 | single-pass direct instance call (`walk_invoke_instance`, the service range at the `pending_staged_arg_oops.push(off)` in its direct arm) | as above | the service, and `receiver_class_id` for kinds 0/2 (`jit_service_callee_deopt_body`) | the service resolves a virtual callee's table from the receiver's class, read out of the copy |
| 3 | single-pass inline caches and the dispatch helper (`jit_invoke_dispatch` / `jit_invoke_virtual_mic`: the args buffer staged at `args_base_offset`, named via `pending_staged_arg_oops`) | the args buffer, and the Rust door's `CompileArgPins` rerun pins (`try_mic_rust_cached_entry`, the MIC / megamorphic arms, `call_compiled_then_route`) | the door's re-run arms (`rerun_declined_callee_from_entry`, `service_frameless_callee_trap`, `try_run_callee_handler`) through `args_now` | the buffer is the helper's INPUT; the pins are what keep it current for the re-run |
| 4 | a direct call inside a spliced body (`x64/inlining.rs`, the two `pending_staged_arg_oops.push` sites) | the splice's service copy | `emit_inline_callee_deopt_check` | not taught the rule (same shape as the fixed one; the splice's `callee_entry` is known) |
| 5 | the optimizing tier's direct call (`jit/src/ir_lower.rs`, `emit_direct_cross_call`) | none: the argument VALUES' homes, which are inputs of the call node, so live at it | `emit_callee_deopt_service_cold` re-stages from those homes after the call | a home can be shared by colours, so it cannot be zeroed without the slot plan's say; the IR tier is off under moving-young (the Generational default), so no probe of this round reached it |
| 6 | interpreted caller into a compiled callee (`vm/src/runtime/interpreter/jit_bridge.rs`, `JitArgPinGuard`, armed after the argument pop loop before `run_jit_body`) | `native_pin_roots` | the `Throw` arm (`jit_saved_args_to_values_pinned`, seeding the handler frame) and the `Stashed` arm (re-run from entry) | the same three readers, on the interpreter's side |
| 7 | aarch64 backend direct calls (`jit/src/aarch64_backend.rs`) | its own service copy | the service | not ported |
| 8 | the fixed shape under `CRATONVM_NO_MOVING_YOUNG` when the spill is elided (`emit_safepoint_metadata_only` records `live_frame_hi = 0` without the moving-young proof) | the popped operand slots of the arguments, now below no claimed cursor | none -- dead words the conservative band scan still reads | the cursor is recorded only on the moving-young arm of `emit_safepoint_metadata_only` (`jit/src/x64/safepoint.rs`, JIT round 13's file this wave) |

## Proposed fix, per shape

- **1:** keep the range for the fallback, but emit the fallback's dispatch
  from the ABI registers (the cell's cold path runs before the callee, with
  the arguments still in them) and drop the range, so the site admits the
  d9/d rule. Measure `TryLoop throw` (the retire cell's own gate) both ways.
- **2:** kind 1 (a private / final `invokespecial` bind) needs no receiver
  class; admit it exactly as the static rule does. Kinds 0/2 need the
  receiver: keep only word 0 (the receiver, which the callee's frame keeps
  alive anyway as `this` while it is live) and drop the others.
- **3:** the Rust doors can release every pin but the receiver's at the
  callee's entry when the callee's body passes `callee_body_owns_its_arguments`,
  with the same frameless hand-off; the args buffer is dead once the helper
  has marshalled it (a post-marshal clear of the buffer words, before the
  compiled callee runs, inside the helper).
- **4:** the fixed rule, in `inlining.rs`, with the splice's own
  `pc_is_protected` answer.
- **5:** zero the argument homes the call's after-call keep set does not
  name, after the ABI marshal and before the `CALL`, and move the cold
  re-stage to the owned-args service word -- the design item (1) of
  `gcd-d4o-proposal-callee-owned-arguments-20260928.md`, now with the
  service half already built.
- **6:** release the non-receiver pins when the callee body owns its
  arguments, and answer the `Stashed` / `Throw` re-runs that would need them
  with the interpreter's own re-execution of the invoke (the caller frame is
  interpreted, so it can re-run the invoke from its operand stack snapshot
  -- which `jit_bridge` would have to keep, un-rooted, only for a precise
  resume).
- **8:** record the cursor on both arms of `emit_safepoint_metadata_only`
  (a JIT round 13 edit), or zero the popped argument slots at an owned site.

## How to verify

`tools/bench/Gcd1ArgPinShapesProbe.java` (this wave): four callers hand a
16 MB list, referenced by nothing else, to a callee that drops it and polls
its `WeakReference` through `System.gc()`:

- `static-direct`: the fixed shape (a warmed caller, a baked direct
  `invokestatic`) -- the control;
- `static-in-loop`: the same call inside a loop in the caller (shape 1);
- `interface-ic`: through an interface call a warmed caller made monomorphic
  (shapes 2/3);
- `interp-to-compiled`: an interpreted caller (it runs once) into the warmed,
  compiled callee (shape 6).

```
javac -d tools/bench tools/bench/Gcd1ArgPinShapesProbe.java
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
for i in 1 2 3; do timeout 300 cratonvm $P Gcd1ArgPinShapesProbe; done
CRATONVM_DBG=oldmark-root-census,root-source timeout 600 cratonvm $P Gcd1ArgPinShapesProbe 2>shapes.log
grep -n 'holder#' shapes.log | grep -v 'Static fields' | head -20
```

HotSpot (`java -XX:+UseSerialGC -Xmx64m`, 3 of 3, and `-Xint` 1 of 1) prints
`PASS static-direct`, `PASS static-in-loop`, `PASS interface-ic`,
`PASS interp-to-compiled`, `PASS all 4`, exit 0. This VM is expected to print
`PASS static-direct` (the fixed shape) and, by reading, `FAIL` on the other
three until their shape is fixed; a `FAIL` line's `holder#1` names the copy:
`region=operand-spill` at the caller (1, 2/3), `cat=.../native-pin` (3 via a
Rust door, 6). Retire each row when its line prints `PASS` 3 of 3 and the
census names no holder at the caller.
