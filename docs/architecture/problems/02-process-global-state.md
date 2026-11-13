# Problem 2: VM state lives in process globals

Filed 2026-11-11 from code analysis only. Revised the same day after reading
`docs/architecture/per-vm-state.md`, which this page had not consulted: the
first version overstated the problem. Counts use the ratchets' own rule (one
per source line that declares a `static`, test modules included).

## What the first version got wrong

`per-vm-state.md` is a reviewed inventory of this exact problem, with fixes.
Three things this page listed as open were already judged there:

- `PROCESS_LIFECYCLE`, `JIT_CODE_RANGES` and the JIT quiescence depth are
  process-scoped on purpose (its section 8: executable pages are an OS
  resource and a grace period must see every thread).
- The `native_override` verdict cache is a pure predicate over names (V18).
- `CLASS_LAYOUTS` (`types/src/field_layout.rs:247`) is indexed by class id but
  every slot has an owner domain (`CLASS_LAYOUT_OWNERS`), so a second VM is
  screened, not misanswered. The cost is real and stays on the list: a
  process with more than one `ClassStore` allocates the legacy 16-byte-cell
  shape (line 1769).

Two gates already freeze the count: `jit/tests/process_global_statics_ratchet.rs`
(`jit/src`, 867) and `vm/tests/per_vm_state_statics_ratchet.rs` (`vm/src`
1526, `classloading/src` 78).

## Problem, as it stands

The audit and both gates cover `vm`, `classloading` and `jit`. The other
runtime crates held about 3,100 statics with no gate and no audit:

| crate | statics |
|---|---|
| native-builtins | 1397 |
| gc | 744 |
| native-api | 238 |
| native-io | 219 |
| types | 207 |
| native-collections | 185 |
| jfr | 92 |
| native-awt | 30 |

Most of what was sampled there is already correct: object-holding state uses
`VmScoped` or a `vm_identity` key, and the TLS tables key through
`gc_stable_lock_key`, which knows its VM. But the bug shape the audit hunts
(`per-vm-state.md` Fact 1: a `ClassId` is an index into its own VM's class
store) was still present twice, found by one grep for statics typed with
`ClassId` and no VM key:

- `native-builtins/src/vector_support_intrinsics.rs` `OP_INFO_SLOT`: a
  thread-local memo `ClassId -> field slot` for `OperatorImpl.opInfo`. A
  second VM on the same thread read the first VM's slot.
- `native-collections/src/lib.rs` `slot_declared_descriptor`: a thread-local
  memo `(ClassId, slot) -> descriptor`. It decides whether the collection
  copy probe may read slot 1 as a size, the check that exists because a wrong
  answer truncated a list silently.

Still open from the audit's own list: V12 (`PROCESS_VM`: one `JavaVM*` per
process, so `AttachCurrentThread` resolves the last VM created), V21 (shared
event-loop manager), V25/V26 (flags and the `env_cache` memos, problem 1).

## Landed 2026-11-11

- Both memos carry `vm_identity` in the key, each with a two-VM test on one
  thread: `op_info_memo_does_not_answer_another_vm` and
  `slot_descriptor_memo_does_not_answer_another_vm`.
- `types/tests/process_statics_ratchet.rs` freezes the eight crates above
  (plus `reader`, `jit-api`, `native-builtins-security`) by the same rule as
  the other two gates.
- At the merge of `dev` `03d235c8c6` the gate did its job once: `dev` had
  added four statics to `native-builtins`. Two were `CLASS_MODULE_SLOT`, a
  process-wide cache of one VM's `Class.module` field slot, one per
  registration site of `Class.getModule`. They are now one thread-local keyed
  by `vm_identity` (`class_module_slot`), with a two-VM test. The baseline is
  1399.

## Plan

1. **Audit the newly gated crates the way section 10 of `per-vm-state.md`
   audited `vm/src`:** every static that keys or holds a `ClassId`, a method
   index, an `ObjectRef` or a loader id, read for what it keys. The grep used
   here only sees a type that names `ClassId` or `ObjectRef`; ids carried as
   `u32` / `usize` and keys built from class names are not covered. Order:
   native-builtins, gc, native-collections, native-io.
2. **V12:** a `JavaVM*` per VM, so a foreign thread attaches to the VM it
   named.
3. **V21:** the event-loop manager per VM.
4. **V26 / V25:** the flag snapshot on `VmConfig`, after problem 1 has made
   reads typed.
5. **Prove it** with one test that boots two VMs in one process in different
   modes (`--jdk-only` and `--compatible`), runs the same class in both, and
   drops one while the other keeps running.

## Done when

- The two-VM test passes under all three collectors.
- Every static in the gated crates has a verdict in `per-vm-state.md`.
