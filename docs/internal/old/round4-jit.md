# Round 4 JIT Performance & Correctness Review

Findings from a deep pass over `jit/src/`, sorted by expected impact.
Items already addressed in rounds 1-3 (PIC rel32 wiring, inline TLAB skip-helper,
FxHashMap migration, MIC inline dispatch, OSR trampoline cache,
`count_param_slots`) and the documented JIT static-field-inline blocker are
excluded.

---

## 1. [HIGH] PIC inline cascade re-loads every callee argument 3 times

**Where:** `jit/src/x64.rs:10582-10599` (inside the `for i in 0..3usize` loop)

The 3-way PIC fast path re-emits the entire arg-marshalling sequence in
every slot:

```
load vm_ptr -> ARG_REGS[0]              ; ~7 bytes
load arg_slots[0..n] -> ARG_REGS[1..n]  ; n * 7 bytes
call qword [r10 + ENTRY_PTR_OFFS[i]]    ; 4 bytes
```

For `n = 4` (typical instance method with three params) that is
`(1 + 4) * 7 + 4 = 39` bytes per slot, replicated 3x. The args are
identical across slots — they should be hoisted **once** before the
3-way cascade, and only the `call [r10+ENTRY_PTR_OFFS[i]]` and the
`jmp .done` should live inside each slot body. This cuts ~78 bytes per
PIC site (slots 1 & 2 redundant loads) and saves icache pressure on
the hot dispatch path.

**Fix:** Move the `emit_load_local(ARG_REGS[0], heap_local_offset)` and
the per-arg load loop above the `for i in 0..3` loop. The slot body
shrinks to: `cmp class_id`, `jne next`, `cmp needs_ctx`, `je miss`,
`call qword [r10+ENTRY_PTR_OFFS[i]]`, `jmp .done`. Same applies to the
MIC inline path at `x64.rs:10681-10685` (less impact: just one copy).

---

## 2. [HIGH] `emit_call_absolute` always emits 12-byte `MOV RAX, imm64; CALL RAX`

**Where:** `jit/src/x64.rs:5203-5210`, 31 call sites across the file

Every helper call costs **12 bytes** even when the helper lives within
+/-2 GiB of the code buffer (the common case on Linux/Windows when the
executable buffer is allocated near the host binary). A `CALL rel32` is
only **5 bytes**.

A typical compiled method has ~10-20 helper calls (array ops, getfield,
write barrier, MIC slow path, deopt stubs), so this is ~70-150 bytes of
wasted icache per method, plus the extra MOV-immediate latency.

**Fix:** After the executable buffer is allocated, compute `disp = helper - (here + 5)`;
if it fits in `i32`, emit `E8 disp32` (5 bytes). Otherwise fall back to
the `MOV RAX,imm64; CALL RAX` form. The helper-to-code distance is
known the moment `ExecutableBuffer::new` returns. Same trick applies to
`emit_jmp_absolute` (`x64.rs:5191`).

---

## 3. [HIGH] Deopt framework is wired but never populated — no frame reconstruction possible

**Where:** `jit/src/lib.rs:433` (`pub deopt_points: Vec<DeoptimizationPoint>`),
`jit/src/deopt.rs` (entire module)

`CompiledMethod.deopt_points` is initialized to `Vec::new()` in `new()` and
`new_with_context()` and **never pushed to anywhere in the crate**. The
deopt stubs at `x64.rs:7122-7193` emit `jit_uncommon_trap(vm,reason,bci)`
and return `i64::MIN`, but they do not record a `DeoptimizationPoint`
with a `FrameState`. When deopt fires (bounds-check failure, div-by-zero,
speculative BCE miss, INT_MIN/-1 fast path), the runtime has no way to
reconstruct interpreter `locals`/`stack`/`monitors` — it just bails to
the caller with the sentinel and the interpreter is expected to
re-execute the entire method from bci=0. This is **functionally a soft
deopt only**.

The penalty: every speculation deopt re-runs from method entry instead
of resuming at the failing bci, throwing away PIC warmup state and
forcing the interpreter to redo work the JIT already did.

**Fix:** In `emit_bounds_check`, `emit_safe_idiv`, and
`emit_deopt_stubs`, build a `FrameState` from the current simulated stack
plus the live `local_assignments` and push a `DeoptimizationPoint` onto a
new `Compiler.deopt_points` field that is moved into `CompiledMethod` at
buffer finalization. `materialize_virtual_objects` (`deopt.rs:566`) also
hands back placeholder addresses starting at `0x1000_0000` instead of
real heap addresses — needs the GC integration the comment describes.

---

## 4. [HIGH] `null_check_elim` analysis runs but its result is never queried

**Where:** `jit/src/x64.rs:11367` (analysis runs), `x64.rs:3189`
(`is_local_nonnull` getter tagged `#[allow(dead_code)]`),
`jit/src/null_check_elim.rs:76` (`analyze`)

`Compiler::is_local_nonnull(pc, local)` is implemented and stored on the
compiler, but **no codegen path calls it**. The analysis walks the
entire method on every compile (allocates a `Vec<u64>` sized to the
bytecode length), but the result is dead.

Today every `aaload`/`iaload`/`getfield`/`arraylength` either (a) crashes
on null via SIGSEGV-as-NPE in the runtime helper, or (b) the inlined
sequence faults on `MOV R10D, [RAX+ARRAY_LENGTH_OFFSET]` (see
`x64.rs:6906` — the bounds-check load of array.length is the implicit
null check). The signal-handler path is slow and brittle on Windows.

**Fix:** Either delete the entire `null_check_elim.rs` module and its
call site (saving compile time), or actually wire it: before
`emit_bounds_check`, when `is_local_nonnull(pc, array_local)` is false
*and* the source slot maps to a local, emit an inline
`TEST RAX, RAX ; JZ deopt_npe_stub` (4 bytes) and add a shared NPE
deopt stub at end-of-method. The analysis becomes load-bearing only
when at least the cheap `aload N` -> `aaload` chain uses it.

---

## 5. [HIGH] `flush_scratch_registers` spills XMM through RAX instead of directly

**Where:** `jit/src/x64.rs:6647-6665`

Every XMM stack slot flush emits two instructions:
```
MOVQ RAX, XMMn     ; 5 bytes
MOV [rbp-off], RAX ; 4-7 bytes (~9-12 total)
```
when a single `MOVQ [rbp-off], XMMn` would suffice (`66 48 0F D6 /r`
form with disp8 = 6 bytes). Called 30+ times per method (see 30 callers
of `flush_scratch_registers`), so this is ~3-6 bytes per XMM flush, plus
the RAX clobber forces other live RAX values to be tracked or spilled.

**Fix:** Add `emit_movq_mem_disp_xmm(off, xmm)` that emits the direct
form `66 48 0F D6` + ModRM[RBP+disp]. Skip the RAX round-trip in
`flush_scratch_registers` and the symmetric XMM restore in
`emit_epilogue` (`x64.rs:5151-5154` does the same RAX/R11 dance on the
restore side).

---

## 6. [HIGH] Prologue uses `MOV [rbp-off], reg` for callee-saved instead of `PUSH`

**Where:** `jit/src/x64.rs:5060-5075` (prologue),
`jit/src/x64.rs:5141-5155` (epilogue)

The prologue does:
```
PUSH rbp                      ; 1 byte
MOV rbp, rsp                  ; 3 bytes
SUB rsp, frame_size           ; 4-7 bytes
MOV [rbp-callee0], R12        ; 5 bytes
MOV [rbp-callee1], R13        ; 5 bytes
...
```

`PUSH R12 / PUSH R13 / ...` is 2 bytes per register, vs 5 bytes for the
MOV form. With 5 callee-saved regs in use (typical for a method with
several locals), the prologue+epilogue burns ~30 extra bytes. PUSH/POP
also doesn't clobber RAX, which matters in `emit_epilogue` where the
return value is in RAX — currently it has to route the XMM restore
through R11 (`x64.rs:5151-5154`) specifically to avoid RAX clobber.

**Fix:** Switch to `PUSH reg` per callee-saved GPR before
`SUB rsp, frame_size`, mirrored by `POP reg` in reverse order in the
epilogue. Adjust `frame_size`/`callee_saved_base` arithmetic to drop the
GPR save area from the explicit frame. This also removes the
`clone()` of `alloc_used_regs` on every prologue/epilogue call.

---

## 7. [MED] `null_thread` slow path always taken when TLS helper fails — no inline TLS

**Where:** `jit/src/x64.rs:5404-5408` (inline TLAB)

Every inline `new` does a full `CALL helpers.get_current_thread` (12 bytes
via `emit_call_absolute` — see finding #2) to fetch the JvmThread*,
followed by `TEST RAX, RAX; JE slow_path`. On Linux x86_64 with native
TLS, the thread pointer is one `MOV reg, FS:[tls_offset]` (about 9-10
bytes) — no call, no null check needed once TLS init has happened.

**Fix:** When `target_os = "linux"` or "freebsd", emit
`64 4C 8B 14 25 <offset32>` (`MOV R10, FS:[offset]`) directly using the
TLS offset of the thread pointer slot. On Windows, `GS:[0x88]` is the
TEB Self pointer; the thread pointer can be read from a slot in the
TEB. This eliminates the call and the null check on the hot allocation
path.

---

## 8. [MED] TLAB cursor 8-byte alignment fix-up runs on every allocation

**Where:** `jit/src/x64.rs:5421-5422`

```
ADD R11, 7         ; 4 bytes
AND R11, -8        ; 4 bytes
```

Runs unconditionally even though the TLAB refill path guarantees an
8-aligned cursor and every allocation rounds `total_size` to a multiple
of 8. The only path that produces a misaligned cursor is primitive
array allocation with a header that doesn't end on an 8-byte boundary —
and `total_size` already includes that header.

**Fix:** Round `total_size` up to a multiple of 8 at compile time
(once, where `emit_inline_tlab_new` is called) and drop the runtime ADD
+ AND. Saves 8 bytes per inline `new` site.

---

## 9. [MED] Per-bytecode env-var lookup on dispatch hot path

**Where:** `jit/src/x64.rs:10346` (inside the `invokevirtual` / `invokeinterface`
emission branch)

```rust
if std::env::var_os("CRATONVM_DBG_JIT_GEN").is_some() {
    eprintln!(...);
}
```

`env::var_os` walks the global env mutex on every virtual call site
compiled. Compile-time perf concern: cold compile of a megamorphic
method with 50 virtual calls = 50 mutex acquisitions, even when the
env var is unset (the common case).

**Fix:** Cache once at the start of `compile()`:
```rust
let dbg_jit_gen = std::env::var_os("CRATONVM_DBG_JIT_GEN").is_some();
```
and pass via a `Compiler` field or a parameter. Same pattern at
`x64.rs:11477`, `11508`, `11526`.

---

## 10. [MED] `emit_mov_reg_reg` has no `dst == src` peephole

**Where:** `jit/src/x64.rs:3630-3634`

```rust
fn emit_mov_reg_reg(&mut self, dst: u8, src: u8) {
    self.rex_w_rb(dst, src);
    self.buf.emit_byte(0x8B);
    self.modrm_reg(dst, src);
}
```

Always emits 3 bytes. The `load_slot_to_reg` wrapper skips it, but
direct callers (LICM hoist code at `x64.rs:7495,7501`, scalar-replaced
field accesses, inlined-callee arg marshalling, prologue param
shuffling at `x64.rs:5084-5087`) emit pointless `MOV rax, rax` when the
ABI register happens to match the assigned local register.

**Fix:** Early-return in `emit_mov_reg_reg` when `dst == src`. Same for
`emit_movsd_xmm_xmm` and `emit_movss_xmm_xmm` (they're called from the
double/float load paths and from `flush_xmm0_slots`).

---

## 11. [MED] OSR trampoline always stores every local to the frame slot, even register-resident ones

**Where:** `jit/src/lib.rs:884-927` (`emit_osr_trampoline` body)

For every local i:
```
MOV RAX, [R10 + i*8]            ; 4 bytes
MOV [rbp - (i+1)*8], RAX        ; 7 bytes
if dst_reg.is_some() { MOV reg, RAX } ; 3 bytes
```

The frame-slot store is unconditional even when the local has a
register assignment — the JIT body will read the register, not the
frame slot. For a method with 8 register-mapped locals, that's
8 * 7 = 56 wasted bytes per OSR trampoline (and an extra L1d store
per local at OSR entry).

**Fix:** When `dst_reg_opt.is_some() || xmm_opt.is_some()`, skip the
frame store. Only emit it when the local is truly frame-resident in
the JITed body.

---

## 12. [MED] Single Compiler bytecode loop does O(N) `Vec::iter().find()` per dispatch lookup

**Where:** `jit/src/x64.rs:9704,9735` (`field_info`), `10319-10345`
(`invoke_info`, `mic_slots`, `pic_slots`), `7903,7918,7933` (`ldc_info`/
`ldc2w_info`), `9271` (`unroll_loops`), `7515,7573` (`simd_loops`)

Each lookup is `Vec<(pc, ...)>.iter().find(|&&(p, _)| p == pc)` — O(N)
in the number of resolved sites. A method with 30 field accesses and
20 invokes does 30 + 20 + (for invokes) 60 more scans (one each for
`invoke_info`, `mic_slots`, `pic_slots`). Compile time, not run time,
but it scales as O(M * N) instead of O(M log N) or O(M).

**Fix:** Build a `FxHashMap<usize, &T>` once at `Compiler::new` (or
inside `compile_bytecode` before the main loop) per metadata vector.
The existing `ldc_map` precompute at `x64.rs:2253` shows the pattern.

---

## 13. [MED] `ir_optimize::gvn` uses `DefaultHasher` (SipHash) on the IR-compile hot path

**Where:** `jit/src/ir_optimize.rs:369`

```rust
let mut hasher = std::collections::hash_map::DefaultHasher::new();
```

`gvn` runs in a fixed-point loop (up to 8 iterations, see
`ir_optimize.rs:16`) and rehashes every live node every pass. SipHash
is ~5x slower than `FxHasher` for small key payloads (op + ty + few
NodeIds). The crate already imports `rustc_hash::{FxHashMap, FxHasher}`
elsewhere — easy switch.

**Fix:** Replace `DefaultHasher::new()` with `FxHasher::default()`.
Also `ir_optimize.rs:6` uses `std::collections::{HashMap, HashSet}`
which fall through to SipHash — use `FxHashMap`/`FxHashSet`.

---

## 14. [MED] PIC `class_id` cascade emits redundant `disp8 0x00` for slot 0

**Where:** `jit/src/x64.rs:10536`

```rust
self.buf.emit(&[0x41, 0x3B, 0x42, CLASS_ID_OFFS[i]]);  // CMP EAX, [R10 + 0/4/8]
```

For `i == 0` (`CLASS_ID_OFFS[0] == 0`), the ModRM byte `0x42` selects
`mod=01, r/m=R10, disp8` and a disp8 of 0 is encoded. The mod=00 form
(`0x02`) elides the displacement byte entirely (3 bytes instead of 4).
Same with `CALL qword [r10+ENTRY_PTR_OFFS[i]]` at line 10599 — but
those offsets are 16/24/32 so the savings don't apply. Slot 0's class_id
cmp is the *only* one that benefits, but the cascade also re-loads
`MOV EAX, [RAX]` at line 10497 from offset 0 — same disp8-vs-disp0 win
(2 bytes vs 1 if we use mod=00 form `0x00` and the receiver class_id
read is shorter).

**Fix:** When emitting `[base + disp]` and `disp == 0` (and base is not
RBP/R13 which require disp8), use the mod=00 form. Helper exists for the
RBP-relative case; add the generic version.

---

## 15. [LOW] `OopMap` precise scan misses register-resident oops

**Where:** `jit/src/x64.rs:3278-3320` (`emit_oop_map_for_safepoint`)

The precise oop map only records `StackSlot::Frame(off)` entries. When
an `aload N` push results in a `StackSlot::CalleeSaved(reg)`
(`x64.rs:8048`), the oop lives in a callee-saved GPR across the safepoint
call. The JIT epilogue restores it from a fixed frame slot
(callee-saved area), so it *is* reachable via the conservative scan,
but the precise map doesn't list it. If the conservative scan is ever
disabled (or moves to a strict precise mode), GC would miss it.

The comment at `x64.rs:3284-3293` acknowledges the fallback. Worth
recording the callee-saved-base slot offset for each `CalleeSaved(reg)`
that holds an oop so the precise map is actually precise.

**Fix:** In `emit_oop_map_for_safepoint`, when
`stack_oop_marks[i] && matches!(stack[i], StackSlot::CalleeSaved(r))`,
look up `r`'s save-slot offset via `alloc_used_regs` and the
`callee_saved_base` and push that frame offset.

---

## 16. [LOW] `ir_lower.rs` uses `disp32` for every frame access regardless of offset

**Where:** `jit/src/ir_lower.rs:136-153`

```rust
fn load_to_rax(&mut self, offset: i32) {
    self.buf.emit(&[0x48, 0x8B, 0x85]);   // mod=10, disp32
    self.buf.emit(&neg.to_le_bytes());    // 4-byte disp
}
```

The disp32 form is 7 bytes; the disp8 form (for `-128..=127` offsets,
which covers any local up to slot 16) is 4 bytes. The IR-routed
backend spills every node to a fresh slot (`alloc_slot`), so loads and
stores absolutely dominate emission cost.

**Fix:** Branch on `neg`: emit `0x45 disp8` when `(-128..=127).contains(&neg)`,
else fall through to disp32. Same for `store_rax`.

---

## Summary

| Severity | Count |
|----------|-------|
| HIGH     | 6     |
| MED      | 8     |
| LOW      | 2     |

Top three by aggregate code-size & runtime impact: **#1** (PIC arg
hoisting — affects every polymorphic dispatch), **#2** (CALL rel32 — every
helper call site), and **#3** (deopt-points wiring — soft-deopt
correctness gap that also wastes interpreter-resume work).
