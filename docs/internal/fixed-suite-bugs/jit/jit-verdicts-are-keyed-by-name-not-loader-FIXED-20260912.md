# FIXED: JIT verdicts and the IR refusal memo were keyed by class name, not by loader

**Status: FIXED 2026-09-12.** This closes the residual of the 2026-09-12 JIT
review finding #55, "Tier state, OSR denials and unload invalidation are keyed
by class name, not loader". The earlier half of that finding (`MethodKey`
carries a `ClassId`) was already fixed. The verdict store is now keyed the
same way.

## The defect

`JIT_VERDICTS` in `jit/src/lib.rs` holds three negative memos: the bail list,
the refusal reason recorded beside it, and the per-(method, pc) OSR entry
rejects. It was keyed by a hash of `(class name, method, descriptor)`, with the
class id fixed at `ClassId(0)`. The IR tier's refusal memo
(`ir_evidence::note_method_refused`) was keyed the same way, plus the redefine
epoch.

Two same-named classes in different loaders therefore shared verdicts: two
webapps, a devtools restart loader, Groovy or JSR-223 scripts. If one copy of
`com.acme.Foo.m()` was bail-listed, the other copy stayed interpreted too.
`forget_jit_verdicts_for_class(name)` dropped every loader's entries, so
unloading or redefining one copy also cleared the other's verdicts.

The result was a method that stayed interpreted, never a wrong answer.

## The fix

The verdict API takes the declaring class's `ClassId` as its first parameter:

- `mark_jit_bail_listed`, `mark_jit_bail_listed_with_site`, `is_jit_bail_listed`
- `record_compile_refusal`, `jit_bail_reason_for` (and the private
  `record_jit_bail_reason`)
- `mark_osr_entry_rejected`, `mark_osr_entry_rejected_by`,
  `is_osr_entry_rejected` (and the private `record_osr_entry_reject`)
- `compile_gate::admit`, which consults the bail list
- `ir_refusal_memo_key(hash, class_id, redefine_epoch)`

Entries are keyed by `(ClassId, class name, method, descriptor)`. Each entry
also stores the id beside the full names, and a hit verifies all four.

`forget_jit_verdicts_for_class(class_id, class_name)` uses the rule
`tiered::MethodKey::belongs_to` uses. When both sides carry a non-zero id, the
id decides. When either side has none, the name decides, which errs towards
forgetting too much.

`TieredCompilationManager::on_class_redefined` now takes `(class_id,
class_name)` too. Its tier-state reset, its OSR-denial purge and its verdict
forget all match with `belongs_to`, as `invalidate_class` already did.

### Where each caller's id comes from

| Caller | Id passed |
|---|---|
| `try_compile_inner` (bail list, bail reason, IR refusal memo), `try_compile_with_invokespecial_resolver`'s `admit` | `cached.declaring_class_id` |
| `compile_osr_artifact` (admit, OSR reject memo, the three OSR bail-list marks, `_with_site`), `try_osr` (`mark_osr_entry_rejected_by`) | the `class_id` parameter |
| `execute`'s eager first-call `admit` (`vm/src/runtime/interpreter.rs`) | `execute`'s `class_id` |
| `try_jit_upgrade_with_gate` (bail check, native-shadow mark, panic note) | `cached.declaring_class_id` |
| `try_jit_compile_callee_uncontained` (bail check, diag reason) | `probe_class_id`, now resolved once at the top and reused by the JIT-cache probe |
| `try_jit_compile_callee`'s panic arm | `get_loaded_class_id(class_name)`, resolved only on that arm |
| `try_jit_compile_callee_slow` refusals after receiver resolution, and its scan-refused bail mark | `callee_class_id` |
| its two refusals before receiver resolution (FJP blocklist, registered native) | `find_unique_class_by_name`, looked up only when the refusal fires |
| its `vm-declaring-class-not-loaded` refusal | `ClassId(0)`: no loaded class exists to key it by |
| background OSR compile (`permanent`, diag reason) | the id `fetch_osr_compile_inputs` resolved, else the key's |
| background compile `declined_permanently` | `task.method_key.class_id` |
| `DeoptimizationController::deoptimize` (`MakeNotCompilable`) | its `get_loaded_class_id` result |
| tiered stats report (`jit_bail_reason_for`) | `state.method_key.class_id` |
| `invalidate_class` | its `class_id` |
| `on_class_redefined` callers: JVMTI `redefine_class` | the redefined `class_id` |
| `on_class_redefined` callers: the three `define_class*` paths and JNI `DefineClass` | the `cid` just defined |

The only `ClassId(0)` left on a production path is the not-loaded refusal
above, which is a diagnostic reason. Tests use `ClassId(0)` where they model
an id-less caller.

The IR site-trap registry keeps `ir_method_memo_hash` un-keyed on purpose. The
runtime looks it up by the names a trapped frame carries.

## Regression coverage

- `jit/src/lib.rs`:
  - `same_named_classes_with_different_ids_keep_separate_verdicts`: two
    same-named classes with different ids keep separate bail-list, reason and
    OSR-reject verdicts. Forgetting one id leaves the other. An id-less forget
    falls back to the name.
  - `the_ir_refusal_memo_key_separates_same_named_classes`
  - `jit_verdicts_are_cleared_with_their_class_and_expire_with_their_epoch`,
    updated for the new signatures.
- `jit/src/tiered.rs`: `redefining_one_loaders_class_leaves_a_same_named_class_alone`.
- `jit/src/compile_gate.rs`: the gate tests pass an id-less `NO_ID`.
- `vm/tests/jit_inherited_callee_constant_pool.rs` and
  `vm/tests/jit_local_exception_handler_tests.rs`: the "not bail-listed"
  assertions now ask about the class id the VM loaded. With `ClassId(0)` they
  would pass without testing anything.

## Not changed

`vm/src/jit/helpers.rs::despeculate_trapped_method` writes
`ir_evidence::note_method_refused(ir_method_memo_hash(..))`, the bare hash. The
reader in `try_compile_inner` looks the memo up under `ir_refusal_memo_key`,
which mixes in the redefine epoch, so that write was already never read back.
This fix did not touch it, because making that write effective would change
tiering behaviour. See the `site_trap` comments there.
