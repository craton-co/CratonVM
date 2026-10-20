# JIT round 21 wave 25: adversarial review of wave 24 (lane review-w25)

Status: OPEN. 10 items. Each item is classified:

- **FIX-NOW:** small and clear, for a lane in this wave.
- **FILE:** stays on this page for the next round.

No code defect was found that needs a FIX-NOW. Item 8 is a doc-only FIX-NOW, and this lane fixed it in
`docs/JIT_OPTIMIZATION.md`. Items 4 and 5 are test gaps (FILE). Item 3 is a known heuristic that a wave-25 lane is
replacing now. The JVMTI question the orchestrator asked (item 6) has a clear answer: `false` in a build without
`experimental-debug` is right.

Scope: `git show 216dcaec7e` (wave 24) plus the build fix `edba4264b7`, read at HEAD 8856941588 with the
concurrent wave-25 edits in the tree. This review only read code. Nothing was built or run, and wave 24 itself is
unbuilt. Functions are named instead of line numbers, because six other lanes are editing the same files.

## Item status

| Item | Severity | Class | Verdict |
|---|---|---|---|
| 1 class-name mirror memo notes only strongly held answers (`class_names_idless`, `class_get_name_memo_fill -> bool`) | Info | FILE (test, memory note) | clean, with notes |
| 2 `bottom_class_hidden(_in)`, `class_hidden_in_store -> Option<bool>`, the five renderers | Info | none | clean |
| 3 JNI re-`<init>` items 1, 2 and 8 | Low (item 2 is still a heuristic) | FILE (being superseded) | clean for items 1 and 8; the item-2 heuristic miscounts a `VmError::Runtime` raised before `super(..)`. Wave 25 lane initwitness (`CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS`, in the tree now) is the exact answer |
| 4 `InvalidationRecord::withdraws_in` and the cache-wide copied-set fallback | Info | FILE (notes) | clean: it only widens, and never past the `=0` rule. One residual miss is unchanged, and the by-id narrowing is mostly given back |
| 5 stale refusal-epoch row clearing | Info (test gap) | FILE | the logic is sound. The new test branch runs only when `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH=1` is in the test environment |
| 6 the JVMTI locals gate; is `false` right without `experimental-debug`? | Info | FILE (doc wording) | right: no C `jvmtiEnv` and no JDWP exist in that build, and the Rust-side locals table has no production caller. In the debug build the gate is a sound superset. The HotSpot rationale in the comments is out of date |
| 7 compact-GC-flag array-header test | Info | none | clean |
| 8 `docs/JIT_OPTIMIZATION.md` flip prose and table lag the flip review page (#7, #13) | Info (doc) | FIX-NOW (doc) | FIXED (this lane): the prose says #13 was fixed in wave 24 (unbuilt) and stays off until the concurrent-mark row has run. Row #7 reads "residual fixed in wave 24, unbuilt"; row #13 reads "defect fixed in wave 24, unbuilt" with status "pending the concurrent-mark row, then A/B" |
| 9 SAFE-TO-FLIP verdicts of `r21-optin-flip-review-20261019.md` after wave 24 | Info | none | none is invalidated (#2, #7, #8, #10 and #14 re-read) |
| 10 `=0` fidelity, lock order, globals, allow-lists, `NativeKind` | Info | none | clean, with one new lock nesting (item 4) |

New switches: none, from this lane or from wave 24. Wave 24 reused `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO`,
`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`, `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME`, `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID`,
`CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` and `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`.

## 1. The mirror memo notes only strongly held answers (Info: clean, with notes)

- **The soundness argument holds.** A line is now noted in two cases only:
  - after `class_get_name_memo_fill` filed a class-id row, whose global root holds the answer;
  - for a mirror with no class id, after `class_get_name_hold_idless_answer` made or found a per-VM global root for
    the answer.

  A hit therefore hands Java a String that a root traces in every phase of every cycle. So the SATB resurrection of
  flip review #13 cannot happen. The leaf path notes only from a class-id memo hit, whose row is that root.
- **Unload and redefine:**
  - A class name does not change on redefinition.
  - An unloaded mirror is freed only by a collection, and every collection moves the address epoch (G1 cleanup and
    the Generational sweep slices included), so the line misses.
  - A class-id row of an unloaded class keeps its String alive (memory only). Class ids are not reused, which flip
    review #12 also relies on.
- **Racing fills:** a second fill of the same class replaces the row and releases the first root. Both fills
  intern the same text. The first String is still rooted when the second interns, so both get the same object, and
  the replacement row holds the very String the first line handed out. A `name` slot written in between changes
  the slot word, and the line misses by its per-hit slot compare.
- **`filed`:** `old != Some(handle)` is right. The old handle is still live when the new one is allocated, so they
  cannot be equal unless the mock arm returned its own handle.
- **Lock order:** `class_names_idless` (`Scratch`) is held only for one `get` (a temporary, dropped at the end of
  the `let`) or one `insert`. `resolve_global_root` and `remove_global_root` run after the guard is dropped. Nothing
  else is acquired under it.
- **`=0`:** with the switch off, `class_names_idless` is never written, and `filed` is computed and ignored. The
  class-id memo behaves as before.
- **Note (memory, FILE if wanted):** the id-less holds are never removed. Arrays of hidden classes that the reverse
  map does not hold add one row and one rooted String each. That is bounded by the number of distinct array texts
  a program asks `getName()` of, so it is not a leak in practice. It is the only growth the fix adds.
- **Test gap (FILE):** the new test drives `class_get_name_idless_hold_lookup` / `_insert` on a hand-built
  `ReflectDoorCaches`. Nothing tests the `held` decision in `native_class_get_name_memoizing` or the new
  `class_get_name_memo_fill` return value. Suggested unit test, if the mock can provide a per-VM
  `reflect_door_cache_slot`:
  1. Fill once for a class-id mirror. Assert `filed` and a line hit.
  2. Fill for an id-less mirror. Assert a hold row and a hit.
  3. Fill for an id-less mirror whose text already holds a different object. Assert no line.

## 2. Bottom-class hidden flag in the message renderers (Info: clean)

- **The guard is right.** `bottom_class_hidden_in` walks `array_info` from whatever id it is given, either the
  class or a component, because a reference array's header carries the COMPONENT id. It then answers only when the
  class it reaches has exactly the name that the descriptor's bottom names. So a mismatched id (a lambda-proxy
  display name, an internal-stamp display name, a primitive bottom) gives `None` and the old shape text.
- **Inputs are internal names:** `array_descriptor_of` builds the descriptor from `c.name` with slashes, and so does
  `cce_display_class_name`. The bottom compare therefore matches the store's spelling.
- **HotSpot:** `Klass::external_name()` runs `convert_hidden_name_to_java` for an `ObjArrayKlass` whose bottom klass
  is hidden. The test's `[Lp.Main$$Lambda/0x1a;` is that spelling.
- **The funnel:** the funnel's shape rewrite (`hotspot_vm_type_error_message`) leaves the new `aastore` texts
  unchanged, as the unit test pins.
- **No nested guards:** every class-manager read is a temporary, or is dropped before `bottom_class_hidden` takes
  its own read:
  - `checkcast_full_path`;
  - the three `aastore` twins;
  - `jni_array_store_message`.

  `vh_array_display_name` and `NativeContextImpl`'s CCE path use the `_in` form under their one read.
- **`raise_could_not_initialize_class_for`:** `Some(true)` keeps the tail as before. `Some(false)` dots the name
  only under the switch. `None` now keeps the shape rule where wave 22 dotted it, which is the fix. Under `=0`, both
  `Some(false)` and `None` reach the shape rule, as before.
- **`=0`:** every new arm goes through `external_name_by_bottom_class_or` or `array_store_element_text`. Both
  return the site's own pre-wave text when `hidden_name_text_by_class_enabled()` is off.

## 3. JNI re-`<init>` items 1, 2 and 8 (Low: item 2 heuristic, superseded in wave 25)

- **Item 1 is right.** `reached` is computed before `ref_processor.lock()`, because
  `jni_reinit_failure_reached_ctor` reads the class manager. The note is then consumed under the lock before the
  "not reached" refusal. With `=0`, `reached` stays `true`, which is the wave-22 path.
- **Item 2 is a heuristic, and it is wrong in one direction.** A `VmError::Runtime` (an NPE, an
  `ArithmeticException`) raised while the constructor computes its `super(..)` arguments leaves before
  `Object.<init>` ran. HotSpot registers no `Finalizer` then, but this rule counts it as reached, so `finalize()`
  runs once more than on HotSpot. The w24 page already filed the exact answer.
- **Superseded:** the tree now contains wave 25 lane initwitness's `jni_reinit_registers_again_witnessed` and
  `jni_reinit_init_return_witness` (`CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS`). This lane did not touch `jni.rs`.
  **Check for that lane's review:** the witness must decide the `Runtime` / `Linkage` arms and the SOE arm, and the
  heuristic must remain only as the fallback when the witness is `None`.
- **Item 8 is right.** `enqueue_unfinalized` returns `false` only for an address already queued, so an accepted
  re-queue is counted exactly once. The counter is per VM, on the `FinalizerThread`.

## 4. `withdraws_in` and the cache-wide copied-set fallback (Info: clean, with notes)

- **It only widens, and never past `=0`.** With `copy_unrecorded`, the by-id branch matches the same `inlined_methods`
  name evidence that the off rule always uses, plus the id evidence. So it can never withdraw more than
  `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID=0` does.
- **Note (perf, FILE):** `copied_bytecode_classes` is never cleared. Once any compile on the cache copied `p/Base`
  (the usual case, because the splice of `p/Base.<init>` is that copy), `copy_unrecorded` is `true` for every later
  withdrawal of that id. So for a body that holds any recorded copy and names a `p/Base.<init>` in `inlined_methods`,
  the by-id narrowing is given back: the other loader's same-named `p/Base` bodies are withdrawn again.
  - The narrowing still holds for bodies whose only evidence is their own `<init>` key.
  - It is fail-closed, so it is no correctness issue.
  - A per-body flag "a copy note was lost during my compile", set where `note_copy_in_open_compile` falls back,
    would restore the narrowing.
- **Residual miss, unchanged (FILE):** a lost copy record for a copy that left no `inlined_methods` triple (an
  IR-tier copy whose splice is recorded only in `copied_classes`) is still missed. The off rule misses it too, so
  the by-id switch is not worse.
- **Lock order (new nesting, note):** `dependencies_current_since` now takes `copied_bytecode_classes` under
  `invalidations`. `copied_bytecode_classes` is a leaf: only `note_bytecode_copied` and `bytecode_was_copied` take
  it, and neither acquires anything under it. So there is no cycle. A comment at the field would keep it that way.
- **Timing:** `invalidate_for_finalizable_subtype` asks for the fallback once, before its walk. A copy noted later
  is judged at publication against the logged record. That is right.

## 5. Stale refusal-epoch row clearing (Info: sound; test gap, FILE)

- **The race logic is sound.** The clearing runs only when the read found a row of another epoch for this
  descriptor. That is possible only under the switch, so `=0` is untouched. Under the write lock it drops the
  descriptor only from a row whose epoch is still `!= epoch` and `< current`. A row that another thread restamped at
  the current epoch is kept. A row restamped at an epoch between the two is stale under `current` anyway, and a
  stale row never answers under the switch.
- **Fail-closed:** the worst case drops a stale row, and the next visit asks again.
- **Lock order:** `finalizer_bars_compiling` runs before the write lock, and no guard is held under the lock.
- **Test gap:** `the_gate_memoizes_a_refusal_per_class_id_and_rereads_the_record` builds
  `SharedVm::new(VmConfig::default())`, and `SharedVm::finalizer_bar_refusal_epoch` reads the opt-in env flag at
  construction. In a default `cargo test` the switch is off, so:
  - the new assertion checks only the off arm (the row stays);
  - the `if shared.finalizer_bar_refusal_epoch { .. }` block never runs.

  **Fix sketch:** build a second `SharedVm` with `finalizer_bar_refusal_epoch = true` (the field is `pub`) and run
  the same redefinition steps on it. Or build both under `with_thread_overrides`, if `SharedVm::new` honours thread
  overrides.

## 6. The JVMTI locals gate (Info: right; doc wording FILE)

**Is `false` right in a build without `experimental-debug`? Yes.** Nothing in that build can read a compiled
frame's locals:

- `vm/src/lib.rs` compiles `pub mod jvmti` (with `native_env`) only under `experimental-debug`;
- `jni_get_env` answers `JNI_EVERSION` with a NULL env for every JVMTI version without the feature, so an
  `-agentpath` agent gets no `jvmtiEnv` at all;
- JDWP is not compiled in: `JDWP_SERVER_COMPILED_IN = cfg!(feature = "experimental-debug")`, and that also makes
  `debugger_observes_locals` `false`;
- the Rust-side `runtime::jvmti::JvmtiEnv::get_local_*` read a side table. The comment at that table says nothing
  outside `#[cfg(test)]` calls them, and `add_capabilities` refuses `can_access_local_variables`;
- `java.lang.instrument` agents have no locals access.

**In the debug build the gate is a sound superset:**

- `potential()` offers OnLoad capabilities only to a startup env (`STARTUP_ENVS` set during `Agent_OnLoad`). In the
  live phase it offers only what `onload_acquired` records, so a live or `GetEnv` env can hold
  `can_access_local_variables` only if the bit is set.
- `bind_startup_envs` records the bit in `Vm::new`, before any Java code runs or compiles.
- `GetLocal*` checks `possesses(CAN_ACCESS_LOCAL_VARIABLES)` and needs the live phase.

**Coverage:**

- every VM `CompileRequest` literal outside `CpResolvers` spreads `..cp_request` (`jit_bridge.rs` sites near the
  three `try_compile_request` calls), so all of them inherit the gate;
- the IR door has one production lowering call, in `ir_tier_attempt`, and it is inside
  `CompileLocalsObservedScope`;
- `try_compile_with_invokespecial_resolver` passes `false`, because it has no VM.

**Notes:**

- (a) `try_lock` contention (an agent thread holding the JVMTI lock while a compile builds its request) declines the
  elision for that compile. This is fail-closed, but it makes the A/B slightly nondeterministic under an agent. It
  is irrelevant without one.
- (b) **Doc wording (FILE):** the comments on `CompileRequest::jvmti_locals_accessible`,
  `locals_capability_may_be_held`, `CompileLocalsObservedScope` and flip review #2 say that HotSpot's escape analysis
  opts out under `can_access_local_variables`. That stopped being true in JDK 16 (JDK-8227745): C2 keeps escape
  analysis and scalar replacement with such an agent, and rematerializes objects through escape barriers (which
  deoptimize the frame) when an agent reaches for a local. The gate here is stricter than HotSpot, which is right
  because CratonVM has no escape barrier. Reword "as HotSpot does" to "HotSpot instead deoptimizes on access (escape
  barriers); with no such barrier here, the elision declines". No Java-visible behaviour changes.

## 7. Compact-GC-flag array-header test (Info: clean)

`r21_flipprep_no_array_header_carries_gc_flag_compact` covers:

- `ObjectHeader::new` for every element type;
- `Heap::alloc_array`;
- the mark and old-gen transitions.

Per its comment, the inline SP and IR allocators take their mark word from `ObjectHeader::new`, so the test also
pins the inline paths at that constructor. A future change to an emitted mark immediate would not be caught. That is
acceptable for a pin.

## 8. The JIT doc's flip prose and table lag the review page (Info, FIX-NOW doc: FIXED)

- **What was stale:**
  - `docs/JIT_OPTIMIZATION.md` "Switch defaults at HEAD" said that #13 "needs a fix first";
  - the table rows read `#7 SAFE-TO-FLIP (residual)` and `#13 KEEP-OFF (fix first) | opt-in, pending fix`.

  The review page itself records #7's residual and #13's defect as FIXED in wave 24 (pending build), with #13
  gated on its missing concurrent-mark row.
- **Fixed (this lane):** the prose sentence and the two rows were updated. The Status column of #13 now reads
  "opt-in, pending the concurrent-mark row, then A/B". No other row changed.

## 9. SAFE-TO-FLIP verdicts after wave 24 (Info: none invalidated)

Wave 24 touched these switches:

- **#2:** gains a decline. The TSO argument is unchanged.
- **#7:** widened only (item 4).
- **#8:** a sound clearing (item 5).
- **#10:** the ASE text of a refused `Object[]` VarHandle store comes from the shared `aastore_store_is_refused`
  helper, which now uses `array_store_element_text`. So the JIT and the interpreter still print the same text.
- **#14 / #15:** test only.
- **#13:** stays KEEP-OFF pending its row.

The other SAFE-TO-FLIP switches' code is not in the wave-24 diff. Their verdicts stand.

## 10. Otherwise (Info: clean)

- **`=0` fidelity:**
  - `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=0`: no hold, no note.
  - `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS=0`: every new arm falls back to the site's old text.
  - `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME=0`: `reached` stays `true`.
  - `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID=0`: `finalizer_copy_unrecorded_on_body` answers `false`, and the off
    rule ignores it anyway.
  - `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH=0`: `row` is never `Some(false)`.
  - `CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED=0`: no JVMTI lock is taken, and the scope is never read.
- **Globals:**
  - `class_names_idless` is per VM, in `ReflectDoorCaches`.
  - `COMPILE_LOCALS_OBSERVED` is a per-compile thread-local scope like `CompileStringLayoutScope`, not compatibility
    state.
  - The re-queue counter is per VM.
- **Allow-lists and `NativeKind`:** no class-name allow-list was added. No native registration changed.

## Suggested probes and unit tests

- **#13 flip gate (still missing, from the flip review):** a loop of `int[].class.getName()`,
  `int.class.getName()` and `Foo[][].class.getName()`, storing each answer into a list that is cleared every N
  iterations, under `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1`, `-Xmx64m`, on Gen and G1, with concurrent marking
  forced. Then `System.gc()` and compare every stored name with `equals`, plus an identity check against
  `String.intern()`. Run it in both `--nojit` and the default.
- **Item 1 unit test:** the funnel-level `held` test from item 1.
- **Item 5 unit test:** the switch-on half of the refusal-epoch test, on a `SharedVm` with
  `finalizer_bar_refusal_epoch = true`.
- **Item 6 probe (debug build only):** an `-agentpath` agent that adds `can_access_local_variables` in
  `Agent_OnLoad`, run with `CRATONVM_DBG_JITC=1` and the elision switch on. No fresh volatile store may be unfenced.
  With no agent, the same kernel must unfence (the w22 `R21Fz9FreshVolatile1` row).
- **Item 3:** the C harness of `r21-finalize-reinit-residuals-FIXED-20261020.md` item 2, with a constructor that throws an
  NPE while it evaluates its `super(..)` argument. On HotSpot it runs `finalize()` once. It needs the wave-25 witness.
