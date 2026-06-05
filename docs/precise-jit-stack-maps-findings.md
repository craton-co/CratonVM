# Precise JIT roots for bintrees18 — investigation findings & shadow-stack status

Branch: `feat/precise-jit-stack-maps` (worktree `C:\craton\CratonVM-pjsm`).
Date: 2026-06-04.

## TL;DR

- **Root cause of the bt18 throughput wall is now precisely understood.** The
  young-gen collector must run the *non-moving* sweep whenever a JIT frame is
  live, because conservative JIT roots can be *marked* but not *rewritten*
  (a stack word that looks like a pointer might be an `i64`). The non-moving
  sweep cannot drain bt18's large long-lived young set → throughput wall (and
  the selective-promotion workaround is buggy: at 8 GB it produces 68332206 via
  a non-moving-sweep + free-block-coalescing interaction, with **zero
  evacuation**).
- **The moving (Cheney) collector is the correct, fast fix and is already
  *correct* for bt18.** `CRATONVM_DBG_FORCE_MOVING=1` runs the moving collector
  under JIT and gives the **golden 67674804 in ~31.5 s** (vs. default hang /
  selective 68332206). It is gated off only because conservative roots aren't
  safely rewritable *in general*.
- **The concrete register-invisibility is an operand-stack oop in a callee-saved
  register.** In `make()`, `aload_1` (pc 12) pushes `n` onto the operand stack
  before `invokestatic make` (pc 16, the GC safepoint); `n` survives the call in
  a callee-saved register and is consumed by `putfield` (pc 19). The existing
  precise machinery spills *locals* to frame slots and maps *frame-slot* operand
  entries, so this **register-resident operand-stack oop is invisible to
  rewriting**. After a move the local copy is fixed but the operand copy stays
  stale → `putfield` writes the wrong node → structurally wrong tree (68199090
  under `PRECISE_JIT_MAPS`, which remaps only 1 of 11 frames).
- **A shadow stack is the right mechanism** (per the chosen direction): push
  every live oop *value* (operand-stack + locals) onto a flat per-thread array
  at each safepoint, reload after the call. The GC scans/rewrites that array
  precisely (every slot is an oop by construction) — no RBP-walk, no per-PC
  oop-map matching, no register-invisibility.

## What is implemented (and its state)

### Stage A — DONE, tested, gate-off safe
- `gc/src/shadow_stack.rs`: `ShadowStack` (per-thread, `repr(C)`, `top` at
  offset 0 for JIT inline access), `push`/`for_each_value`/`remap`/`reset`.
  3 unit tests pass.
- `JvmThread.shadow_stack` field + `JvmThread::shadow_stack_offset()`.
- Marking hook (`vm/src/memory/roots.rs`): folds shadow values into roots.
- Post-move remap hook (`vm/src/memory/gc.rs`): `thread.shadow_stack.remap(pointer_map)`.
- `set_jit_thread` lazily `ensure_allocated()`s the shadow stack.
- Gate `CRATONVM_SHADOW_STACK`; gate-off path is byte-identical.

### ✅ RESULT (commit d69edda): bt10/16/18 ALL GOLDEN under the shadow stack
- `bt10=135854`, `bt16=14985902`, `bt18=67674804` with `CRATONVM_SHADOW_STACK=1`;
  bt18 ~21.5 s (drains young — throughput wall solved). `DBG_SHADOW` shows the
  moving-GC remap precisely relocating JIT-held oops (`rewritten=38`, `=57`).
  Gate-OFF is byte-identical (default unbroken). The two push/reload bugs below
  were found and fixed:
  1. **Unbalanced-push leak** — `make()`'s `invokespecial <init>` safepoint
     pushes a live oop with no paired reload (home-dump: `pc4 homes=[Frame(40)]`),
     leaking the shadow `top` one slot per `make` call → buffer overflow → OOB
     write. Fixed by the **per-method watermark**: prologue saves `top`, epilogue
     restores it (null-guarded) — unwinds the leak on return, correct under
     recursion. (`Node.<init>` is NOT inlined — the earlier assumption was wrong.)
  2. **OSR entry** bypasses the prologue thread-fetch → garbage (non-null) thread
     slot the null-guards can't reject → crash. Fixed VM-side: the OSR trampoline
     (`emit_osr_trampoline`) zeroes the thread slot (clobber-free
     `MOV qword [rbp-off],0`) so OSR-entered frames cleanly SKIP shadow tracking;
     `CompiledMethod.shadow_thread_slot_off` threads the offset through
     `osr_enter → osr_trampoline`.
- Remaining for full generality (not blocking bt18): OSR-frame *tracking* (store
  the real thread ptr at OSR instead of zeroing) so OSR'd methods' oops are
  remapped under GC rather than relying on the from-space staleness window;
  regression pool + perf; multi-thread shadow scan; then default-on.

### Stage B — (historical) codegen written; crash bisected and fixed (see above)
- `jit/src/x64.rs`: `emit_shadow_push` / `emit_shadow_reload` (push live oop
  values via `[thread+shadow_off+TOP]` using R10/R11/RAX scratch; reload writes
  back to home reg/frame slot). Thread pointer cached in a prologue-set frame
  slot. Null-thread guards on all sites. OSR-entry thread re-fetch.
  `ShadowHome` enum, helper-table field `shadow_stack_offset_in_thread`.
- Bisect toggles: `CRATONVM_SHADOW_NOPUSH`, `CRATONVM_SHADOW_NORELOAD`,
  `CRATONVM_DBG_SHADOW`.

### Stage C — gate flip done
- `gen_heap.rs`: when `CRATONVM_SHADOW_STACK` is set, the quiescence gate allows
  the moving Cheney cycle under JIT.

## Current blocker (the runtime crash, well-characterized)

With `CRATONVM_SHADOW_STACK=1`, bt10/16/18 SEGV. Bisected via the toggles:
- `NOPUSH` (prologue thread-fetch + gate flip, no push/reload) → **golden** →
  prologue/gate/null-guard/marking/remap are all sound.
- Full push/reload → crash. The crash is a **data corruption without any GC**
  (no remap line; crashes even at 30 GB heap), i.e. push/reload writes a wrong
  value into an oop home, later deref'd by a VM helper (`jit_getfield`-class)
  → access violation; after the OSR re-fetch was added it became a stack
  overflow (the OSR fetch destabilized loop control flow).

### Crashes already fixed during the investigation
1. `get_current_thread` returns **null** in some JIT prologues (inline-TLAB
   guards this; we didn't) → `[null+0x198]` fault. Fixed with null guards.
2. A per-method **watermark** (prologue save / epilogue restore of shadow `top`)
   was added to clean unbalanced pushes, but it is reached by **OSR/alternate
   entries** with an uninitialized thread slot → wild write. **Reverted**
   (it was also unnecessary: `Node.<init>` is an empty ctor → inlined → no
   unbalanced safepoint).

### Likely remaining causes (next session)
- **Push/reload register/value correctness** under the real register allocator
  for the recursive `make`/`check` shapes — needs a *step debugger* on the
  emitted code (hs_err backtraces were insufficient). Candidates: a home
  register the reload writes is also relied on by post-call code; a category-2
  (long) operand entry mis-typed as an oop in `stack_oop_marks`; pending/top
  pairing across inlined or unbalanced safepoints.
- **Every non-prologue JIT entry** (OSR — partially handled; any deopt/tail-call
  re-entry) must initialize the thread-slot, OR the slot must be zero-init at
  frame setup so the null-guard skips. Cleanest: VM-side OSR setup writes the
  thread pointer to the frame slot (expose `shadow_thread_slot_off` on
  `CompiledMethod`) instead of a mid-loop `get_current_thread` call.

## Recommended path
1. Drive the push/reload bug with a step debugger (WinDbg/x64dbg) on a single
   JIT'd `make`, comparing emitted bytes against intent at one safepoint.
2. Move thread-slot init off the hot path: VM-side OSR setup writes it; the
   prologue keeps the normal-entry write.
3. Once bt10/16/18 are golden under `CRATONVM_SHADOW_STACK`, validate the
   regression pool (14 apps) + measure push/reload overhead, then consider
   default-on (and multi-thread shadow scan in `roots.rs`/`gc.rs`, currently
   current-thread only — matches the existing conservative scan's limitation).

## Interim option
`CRATONVM_DBG_FORCE_MOVING=1` already gives golden bt10/16/18 (bt18 ~31.5 s) by
running the moving collector under JIT with conservative roots. It is correct
for these workloads but not provably safe in general (a conservative false
positive could be rewritten, or a register-only oop missed). The shadow stack
is the principled generalization that makes it safe for all workloads.
