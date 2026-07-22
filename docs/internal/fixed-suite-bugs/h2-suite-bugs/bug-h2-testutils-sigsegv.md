# H2 — `TestUtils` JIT `EXCEPTION_ACCESS_VIOLATION` (aastore SATB barrier)

## Status
**FIXED** (worktree `fix/h2-suite-loop`, `jit/src/x64.rs`).

## Severity
**HIGH** — fatal process crash; JIT-only, deterministic. Affects **any**
JIT-compiled method whose only heap-touching opcode is `aastore` (ref store
into an `Object[]`).

## Affected test class
`org.h2.test.unit.TestUtils` (crashes deterministically under JIT; `--nojit`
runs without the crash).

## Symptom
```
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=... (RVA 0x1F3A5F)
  cratonvm_gc::vm_heap::VmHeap::satb_barrier        [gc/src/vm_heap.rs:844]
  cratonvm_vm::jit::helpers::jit_satb_pre_write_barrier [vm/src/jit/helpers.rs]
```
Crash registers showed `rcx = 0x4CA4A810` — a **stack** address
(`rsp = 0x4CA4A5D0`), not a valid `SharedVm` pointer.

## Root cause
`aastore`'s JIT codegen (`x64.rs`, opcode `0x53`) emits the SATB pre-write
barrier and the post-store write barrier, both of which load the VM pointer from
the frame's `heap_local_offset` slot:
```
emit_load_local(ARG_REGS[0], self.heap_local_offset)  // expects vm_ptr
... call jit_satb_pre_write_barrier(vm_ptr, old_ref)  // does vm.heap.satb_barrier(...)
```
But the JIT-eligibility pre-scan that computes `needs_heap` did **not** set it
for the array-store opcodes (`0x4f..=0x56` just did `pc += 1`). When a method
contains an `aastore` and no *other* heap opcode (putfield/invoke/new/…),
`needs_heap` stays `false`, so `heap_local_offset = 0` (aliasing local 0) and the
slot is never initialised with the `SharedVm` pointer. The barrier therefore
passes a **stack address** as `vm_ptr`; `jit_satb_pre_write_barrier` does
`vm = &*(vm_ptr as *const SharedVm); vm.heap.satb_barrier(...)`, dereferencing
garbage → SIGSEGV.

## Fix
In the `0x4f..=0x56` array-store arm of the `needs_heap` pre-scan, set
`needs_heap = true` for `aastore` (`op == 0x53`). The primitive array stores
(`iastore`…`sastore`) do inline stores with no heap-dependent helper, so they
stay out of `needs_heap`.

Verified: `TestUtils` runs under JIT without the access violation; `--nojit` was
already crash-free.

## Repro
`org.h2.test.RunOne org.h2.test.unit.TestUtils mem` (JIT on) — or any tiny
JIT-compiled method that does only `arr[i] = ref;`.
