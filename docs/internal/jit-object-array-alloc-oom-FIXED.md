# JIT `new` / `anewarray` heap exhaustion → catchable OutOfMemoryError + negative-length → NegativeArraySizeException (FIXED)

**Status:** FIXED. Branch `fix/jit-object-alloc-oom`, merged to `dev`. Follow-up
to [jit-newarray-oom-as-sigsegv-FIXED.md](jit-newarray-oom-as-sigsegv-FIXED.md)
(which covered the primitive `newarray` 0xbc path).

## What was wrong

`jit_new_object` (0xbb) and `jit_anewarray_object` (0xbd) allocated via the
**non-fallible** `alloc_object` / `alloc_array`, which spill young → old gen and
then **hard-abort the process** in `alloc_young` on true exhaustion — instead of
throwing a catchable `java.lang.OutOfMemoryError` like HotSpot. Separately, both
helpers returned the bare `0`/null sentinel on a **negative array length**, so:

- `new Object[-1]` (anewarray) → null pushed → `arraylength` SIGSEGV.
- `new int[-1]` (newarray, after the prior bail shipped) → silently returned 0.

HotSpot throws `NegativeArraySizeException` (message = the length) for both.

## Fix (`gc/src/gen_heap.rs`, `gc/src/vm_heap.rs`, `vm/src/jit/helpers.rs`, `jit/src/x64.rs`)

1. **Fallible-with-old-gen object alloc.** New `GenHeap::try_alloc_object_full`
   mirrors `alloc_object`'s `young → try_alloc_object_old` spill but returns
   `None` instead of aborting in `alloc_young`. Exposed on the `VmHeap` enum
   (Generational → the new method; G1 → its `try_alloc_object`). Arrays reuse the
   pre-existing `try_alloc_array_full` (young → humongous/old → None).
2. **Helpers go fallible.** `jit_new_object` → `try_alloc_object_full`,
   `jit_anewarray_object` → `try_alloc_array_full`; on `None` both call
   `jit_alloc_oom` (stashes `OutOfMemoryError` in `JIT_PENDING_EXCEPTION`,
   returns 0). The old-gen spill of the non-fallible path is preserved.
3. **Negative length → NegativeArraySizeException.** New `jit_negative_array_size`
   helper (sibling of `jit_alloc_oom`) stashes a
   `NegativeArraySizeException(length)` and returns 0; `jit_newarray` /
   `jit_anewarray_object` route negative lengths to it (reordered after the
   `vm_ptr` check so `vm` is available to build the exception).
4. **Codegen bail.** `emit_post_alloc_oom_check` (TEST RAX,RAX; JZ → shared
   exception stub) is now emitted after the `new` (0xbb, non-scalar-replaced
   arm) and `anewarray` (0xbd) helper calls — it already covered `newarray`.
   This forces `has_dispatch` (via `emitted_alloc_oom_check`) so the per-thread
   TLS is set (GC + exception construction work) and the interpreter's
   general-exception drain routes the stashed throwable through the method's
   exception table (catchable). The scalar-replaced `new` arm has no alloc and
   is untouched. The inline-TLAB `new` fast path can't OOM (it only commits when
   the TLAB has room); its slow path routes through `jit_new_object`, so the
   single merge-point bail covers both.

## Validation (== HotSpot)

- `ObjAllocOom.objArray(100_000_000)` (JIT anewarray) → `caught OutOfMemoryError`.
- `NegArray`: `new int[-1]` and `new Object[-1]` → `NegativeArraySizeException msg=-1`
  (was silent-0 / SIGSEGV).
- Old-gen fallback + correctness preserved: `bintrees10/14/18` = 135854 /
  3222190 / **68332206** (bt allocates millions of `new` TreeNodes, promoting
  long-lived ones to old gen). Prior fixes intact: `BigArrayOom` caught,
  `IrCallGc`@32m correct, `IrCall` inc-22 gate off+on 23762906400000.

### Blast radius

Only methods that emit a primitive `newarray`, `anewarray`, or non-scalar-replaced
`new` become `has_dispatch`. Real `new X()` already is (the `invokespecial <init>`
forces it), so in practice only bare `anewarray` / constructor-less allocators
change tier — and they need the thread for GC anyway. bt18 golden unchanged.

## Known separate limitation (NOT this fix): OOM when the heap is 100% full

A retained-allocation loop that fills the **entire** heap (`ObjAllocOom.objLoop`)
still **aborts** — not at the object allocation (that is now fallible and
correctly detects exhaustion) but downstream, when **materializing the
`OutOfMemoryError` object itself**: `create_exception_object` allocates via the
non-fallible `alloc_object`, which `alloc_young`-aborts because there is no room
left for even the ~104-byte throwable. This is the classic "OOM-during-OOM"
problem and is **VM-wide**: the *interpreter* aborts identically
(`CRATONVM_JIT_THRESHOLD=2000000000` reproduces it), because its
`alloc_object_shared` is already fallible/catchable but the exception
materialization is not. The robust fix is a **pre-allocated/singleton
`OutOfMemoryError`** (HotSpot's approach); no such infra exists today. Tracked as
a separate follow-up. This fix neither causes nor worsens it (before, the object
allocation itself aborted one step earlier).

`jit_alloc_oom`'s doc already promises a graceful 0-return "if the OOME object
cannot be constructed" — that path is currently unreachable because
`create_exception_object` aborts rather than returning `Err` on a full heap;
making exception materialization fallible is part of the pre-allocated-OOM work.
