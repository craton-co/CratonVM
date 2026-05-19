# JIT LoopTest Regression — Blocker Report

Status: Partial fix landed in worktree (NOT committed). Root cause of original
"silent hang" identified and fixed for 1-array-param methods. A SECOND, deeper
bug in `x64::compile` blocks LoopTest itself (3-array-param method).

## Original hypothesis (from prior agent) was WRONG

The continue_prompt asserted the bug was in `jit/src/x64.rs::emit_simd_int_array_element_wise`
double-emitting the loop body. I verified this is NOT the cause:

- Gating `if has_avx2()` to `if has_avx2() && env::var("DISABLE_SIMD_EWISE").is_none()`
  at line 7204 (skipping all SIMD-ewise emission) reproduces the SAME hang/crash.
- The SIMD preheader correctly advances `R10D` to `n` and the codegen properly
  syncs it back to the iv local; the `if_icmpge exit` at the natural header PC
  then jumps to the loop exit. Scalar fall-through is unreachable.
- The `simd_loops` (int-array-sum) path, `simd_fp_loops`, and `loop_unswitch`
  preheaders are also gated; LoopTest's `add` method only matches the ewise
  pattern (`simd_ewise=1`, `simd_loops=0`).

## Root cause #1 (FIXED, not committed)

`vm/src/runtime/interpreter.rs::execute_jit_call` (the hot path that runs
already-JIT'd methods invoked via the interpreter's invoke-cache) was popping
operand stack values via `ValueStack::pop_raw()`, which returns the raw
NaN-boxed `CompactValue` bits. For object/array references, that returns
`0xFFFD_xxxx_xxxx_xxxx` (NaN-box | `SUB_OBJECT` tag). The JIT then loads this
value into the argument register (e.g. `RDX` on Windows), and the very first
`arraylength` / `iaload` (`mov eax, [rax + 0x0C]`) faults reading kernel
address space → silent process exit on Windows (no Java exception, rc=0).

The "more than once" symptom was misleading. The first call to `add()` is
JIT'd via OSR — OSR uses `frame.get_local_raw()` which stores object refs
UNTAGGED (see `types/src/value.rs::encode_value` returning `(r.as_ptr() as u64, VTAG_OBJECT)`).
So OSR works on the first call. The second call goes through the invoke-cache
fast path → `execute_jit_call` → `pop_raw()` (BUG) → tagged ptr → SEGV.

Symmetric repros that have nothing to do with the SIMD ewise pattern or even
loops:

```java
// crashes after JIT promotion (~2000th call):
public static int sum2(int[] a) {
    int n = a.length;
    dummy = 1;          // force needs_heap (non-IR backend)
    return n;
}
```

Workaround `RUSTJVM_DISABLE_JIT=1` works because the JIT entry isn't taken.

### Fix in worktree (uncommitted, from prior agent's stash)

- `vm/src/runtime/value_stack.rs`: added `ValueStack::pop_jit_arg()` that
  unboxes by `CompactTag`:
  - `Int` → low 32 bits zero-extended
  - `Float` → bit pattern
  - `Long`/`Double` → raw bits via `as_long_unchecked()`
  - `Object` → `as_object_ptr()` (47-bit pointer, tag stripped)
  - `Null`/`Uninit`/`ReturnAddress` → 0
- `vm/src/runtime/interpreter.rs::execute_jit_call`: replaced `pop_raw()` with
  `pop_jit_arg()`.
- 4 unit tests in `value_stack.rs` cover object tag stripping, int low-32,
  long/double raw bits, and null.

### What this fixes

| Test                          | Before  | After    |
|-------------------------------|---------|----------|
| LoopTest11 (invoke + array)   | SIGSEGV | OK       |
| LoopTest13 / 14 / 15 / 16     | SIGSEGV | OK       |
| LoopTest17 (1 int[] param)    | SIGSEGV | OK       |
| LoopTest18 (1 int[], k-trace) | SIGSEGV | OK       |
| **LoopTest (3 int[] params)** | hang    | **SIGSEGV** (still broken — see below) |
| **LoopTest6 (3 int[] params)**| SIGSEGV | SIGSEGV (still broken) |

## Root cause #2 (UNRESOLVED — blocks LoopTest itself)

Methods with **3 array parameters** (e.g. `add(int[] a, int[] b, int[] out)`)
crash inside the x64 JIT body even after the args arrive as clean pointers.
Verified via `RUSTJVM_DBG_EXECJIT=1`:

```
[EXECJIT] LoopTest6.add([I[I[I)I: arg[2] kind=[ raw=0xfffd0000281d3e50 -> 0x281d3e50
[EXECJIT] LoopTest6.add([I[I[I)I: arg[1] kind=[ raw=0xfffd0000281d3ca0 -> 0x281d3ca0
[EXECJIT] LoopTest6.add([I[I[I)I: arg[0] kind=[ raw=0xfffd0000281d3af0 -> 0x281d3af0
[SIGSEGV]
```

The three array pointers are stripped of their tag bits and look clean
(`0x281d3xxx`). The JIT'd code then segfaults somewhere inside the loop.

Disabling the SIMD-ewise preheader does NOT change this — the scalar JIT body
also crashes for the 3-array-param shape. LoopTest5 (same shape, 100 elements,
direct JIT promotion not OSR) WORKS. So the bug is in the interaction between:
- 3 array parameters arriving in `RDX/R8/R9` (needs_heap shifts ARG_REGS by 1)
- OSR or the post-prologue regalloc state for that param count.

Next agent should:
1. Build a minimal repro `apps/min_3arg_crash` — single-method `int f(int[] a, int[] b, int[] c)`
   with a putstatic and a loop — and check whether disabling the entire SIMD
   preheader block (lines ~7050-7250 in `jit/src/x64.rs`) makes it work. If so,
   the bug is a preheader interaction. If not, it's in the scalar codegen.
2. Use `RUSTJVM_DBG_JIT_DUMP=1` (gated dumper still present at line 10539 of
   `jit/src/x64.rs` — easy to re-add) to capture the emitted asm and diff
   between LoopTest5 (works) and LoopTest6 (crashes). Both compile through
   `x64::compile`; the difference is needs_heap + the larger frame.
3. Specifically check: with needs_heap=true and 3 array params, the prologue
   stores `a → RBX`, `b → R15`, `out → R14`, `vm_ptr` to a frame slot, then
   the SIMD preheader at line 7212 does `mov rax, RBX; mov rcx, R15; mov rdx, R14;
   mov R10, R12_iv; mov R11, R13_n`. The SIMD body then pushes/pops R12/R13
   even though they're already in use as iv/bound locals. The PUSH happens
   AFTER `mov R10, r12` so r10 has the iv value, but the subsequent `mov R12,
   R10; shl R12, 2; add R12, RCX; ...` rebuilds `&b[i]` in R12 — clobbering the
   live `iv_local`. The POP R12 at the end restores it. **BUT** the JZ-skip
   path (chunks==0) bypasses both PUSH and POP, so r12 is preserved. So this
   looks correct on paper.

## Files touched in this worktree (uncommitted)

- `vm/src/runtime/interpreter.rs` — `execute_jit_call` uses `pop_jit_arg`
- `vm/src/runtime/value_stack.rs` — `pop_jit_arg()` + 4 unit tests
- `test_classes/gpu_smoke/LoopTest{2..18}.{java,class}` — narrowing repros
  (LoopTest6, LoopTest11..18). LoopTest5 / LoopTest9 / LoopTest10 / LoopTest12
  all PASS without `RUSTJVM_DISABLE_JIT=1`.

## Constraints respected

- No changes to `cuda-bridge/`, `jit-cuda/`, `native-builtins/`, `apps/`.
- No additions to `jit/src/skip_list.rs` — root cause is being chased, not papered over.
- No synthetic shims.

## Bench-4way and jit-cuda test results NOT RE-RUN

Because LoopTest itself still SEGVs, `bench-4way.sh` would still show a FAIL
row for `cratonvm_cpu_jit_on`. Re-running it before the second root cause is
fixed would be wasted minutes — defer until LoopTest's 3-array-param crash is
resolved.
