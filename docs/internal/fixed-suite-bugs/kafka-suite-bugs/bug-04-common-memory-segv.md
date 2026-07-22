# Bug 04 — SIGSEGV in `GarbageCollectedMemoryPoolTest` (forEach holds raw refs across GC)

**Severity:** Critical — native access violation crashes the process (rc=139).
`--nojit`, interpreter path. **CratonVM-only** (HotSpot runs all 13 tests clean).

**Repro:** run the whole `org.apache.kafka.common.memory.GarbageCollectedMemoryPoolTest`
class (the only class in `common.memory`). Every method passes individually; the
SIGSEGV only appears in the full-class run, which exercises `System.gc()` +
`PhantomReference` reclaim (`testBuffersGarbageCollected`) alongside the others.

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=... faulting RVA 0x89B6EF
   cratonvm_vm::vm::vm_exec::invoke_virtual            (vm_exec.rs:4157)  <- class_id_of(receiver) on a stale ptr
   cratonvm_native_collections::native_al_for_each     (lib.rs:6386)
   ...safe_native_call -> try_stackless_invoke -> execute_invoke_kind -> execute_frame
```

## Root cause (FIXED)

`native_al_for_each` (the `ArrayList`/`Collection.forEach` native) captured the
`action` consumer and the collected element `Value`s as **raw `ObjectRef`s**, then
called `action.accept(elem)` via `invoke_virtual` in a loop. Each `accept()` body is
arbitrary Java bytecode that can allocate → the moving young GC relocates `action`
and the object-typed elements. Those raw Rust-side refs are **not GC roots**, so they
are not forwarded; the next `invoke_virtual(action, …)` dereferenced a stale pointer
in `class_id_of` → SIGSEGV. (Same family as the documented Cleaner / classloader
GC-root gaps.)

**Fix** (`native-collections/src/lib.rs`, `native_al_for_each`): pin `action` and the
object elements with `pin_native_root`, and re-read the forwarded refs via
`read_native_pin` on every iteration (unpin with `unpin_native_roots`). This is the
in-tree contract for natives that hold refs across re-entrant/allocating calls.

Verified: the full class no longer crashes (13 tests run); `forEach` returns correct
results vs HotSpot; serialization/config packages unchanged (no regressions).

## Remaining (separate, lesser)
`testBuffersGarbageCollected` still reports **1 failure** (vs HotSpot 0): it asserts
the pool reclaims buffers after `System.gc()` + a `Cleaner`/`PhantomReference` runs.
CratonVM's reclaim timing/semantics differ — a behavioral GC-reclaim difference, not a
crash. Tracked separately from the SIGSEGV (the reported bug), which is resolved.

> Note: `native_map_for_each` (and other forEach-style natives that loop
> `invoke_virtual` over collected refs) share the same raw-ref-across-GC pattern and
> should get the same pin/re-read treatment in a follow-up.
