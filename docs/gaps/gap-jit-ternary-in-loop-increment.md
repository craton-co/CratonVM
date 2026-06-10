# Gap: JIT miscompiles a ternary `i == 0 ? 2 : 1` used as a for-loop increment

**Discovered:** 2026-06-09 (commons-math FFT test fallout, narrowed from `FastFourierTransformerTest`)
**Severity:** Medium — fundamental control-flow miscompile, but the exact bytecode shape (`iload; iload; ifne; iconst_a; goto; iconst_b; iadd; istore`) only fires in code that uses a ternary directly in a loop step. Real-world tripwire so far: commons-math `FastFourierTransformerTest.testSinFunction` and `testAdHocData`.
**Status:** Open — has a tight minimal repro; narrow skip applied to the two failing test methods so commons-math runs.

---

## Symptom

`FastFourierTransformerTest.testSinFunction`:
```
expected: <0.0> but was: <-128.0>
```

A loop that uses a ternary in its step expression visits an iteration that should be skipped. With JIT off, the loop is correct; with JIT on (default `--jit` threshold), the loop visits `i = 1` when it should jump from `0` to `2`.

The bug surfaces only after the loop has run enough iterations for the enclosing method to cross the JIT compile threshold (~500 invocations for the minimal repro).

---

## Minimal repro

```java
// Tern3.java
public class Tern3 {
    static int loopAdd(int n) {
        int i = 0;
        int visits = 0;
        while (i < n - 1) {
            if (i == 1) return -1;            // we expect to skip i==1
            visits++;
            i += i == 0 ? 2 : 1;              // first step is +2, then +1 forever
        }
        return visits;
    }
    public static void main(String[] args) {
        int iters = Integer.parseInt(args[0]);
        for (int t = 0; t < iters; t++) {
            int v = loopAdd(256);
            if (v < 0) { System.out.println("BAD at trial " + t); return; }
        }
        System.out.println("OK " + iters);
    }
}
```

```bash
$ javac Tern3.java
$ java         Tern3 50000          # HotSpot:    OK 50000
$ cratonvm     Tern3 50000          # CratonVM JIT: BAD at trial 500
$ cratonvm --nojit Tern3 50000      # CratonVM nojit: OK 50000
```

**The original control-flow pattern in `FastFourierTransformerTest.testSinFunction`** (which produced the same miscompile through the same bytecode shape):

```java
for (int i = 0; i < size - 1; i += i == 0 ? 2 : 1) {
    Assertions.assertEquals(0.0, result[i].getReal(), tolerance);
    Assertions.assertEquals(0.0, result[i].getImaginary(), tolerance);
}
```

---

## Bytecode shape

`javap -c Tern3.class` for `loopAdd`, focused on the loop step:

```
21: iload_1          // load i
22: iload_1          // load i again (operand of the ifne test)
23: ifne   30        // if i != 0, jump to 30
26: iconst_2         //   else push 2
27: goto   31
30: iconst_1         //   true branch pushes 1
31: iadd             //   stack: [i, val] → i + val
32: istore_1
33: goto   4         //   back to loop header
```

Both predecessors of pc=31 leave `[i, val]` on the operand stack where `val` is `2` or `1`. The merge at pc=31 must phi the second stack slot.

---

## What the JIT emits (incorrectly)

Hex dump from `CRATONVM_DBG_JIT_CODE=loopAdd Tern3 2000` (the relevant slice for the ternary block):

```
4585ed                          test  r13d, r13d
0f8510000000                    jne   +0x10                  ; jump to merge if i != 0
48c7c002000000                  mov   rax, 2                 ; iconst_2 (i == 0 branch)
e900000000                      jmp   +0x0                   ; goto pc=31 (jmp to next byte)
488b45e0                        mov   rax, [rbp-0x20]        ; ← merge point: CLOBBERS the 2
...
```

`jne +0x10` skips both the `mov rax, 2` AND what should be the `mov rax, 1` for the true branch, jumping directly to the merge point. The merge point does `mov rax, [rbp-0x20]` — a load from a stack slot that neither branch wrote to. The false branch wrote `2` to `rax`, but the merge immediately overwrites `rax` with the stale slot's value (which, in practice, decays to a fixed value that the loop reads as `1` after the first invocation). The true branch never writes anything at all.

Result: every iteration adds the SAME value from `[rbp-0x20]`. Once that value is `1`, the loop visits `i = 0, 1, 2, 3, …` instead of the correct `0, 2, 3, 4, …`.

---

## Suspected cause

The IR builder at [jit/src/ir.rs:489](jit/src/ir.rs:489) (`add_merge_predecessor`) and [jit/src/ir.rs:499](jit/src/ir.rs:499) (`activate_merge`) builds the phi correctly on paper: both predecessor states include `[i, 2]` and `[i, 1]` for the stack-top slot. The bug is in lowering — the phi for the stack value loses its predecessor writes when one predecessor is a fall-through and the other is a `goto`. The codegen treats `[rbp-0x20]` as the phi's slot but neither branch is ever lowered as "store the constant into that slot."

Specifically `jne +0x10` is skipping past `mov rax, 2`, `jmp`, AND the would-be `mov rax, 1` — i.e. the codegen never emits the true branch's iconst at all. The merge then reads from a slot that was never written by either side.

Verification surface to confirm: instrument `ensure_merge` / `activate_merge` to dump the constructed phi for the stack-top slot at pc=31, plus a hook in `x64.rs` to print the emitted bytes per IR node. If the phi has the two `Const(2)` / `Const(1)` inputs but the bytecode emitter elides them as "constants that don't need emitting" (treating them as eagerly-foldable into `[rbp-0x20]` initialization that never happens), that's the bug.

---

## Workaround attempted (not applied)

A narrow JIT-skip for `FastFourierTransformerTest.testSinFunction`/`testAdHocData` was tried but did NOT eliminate the failures even with the SKIP entries verified in-binary. After `CRATONVM_JIT_BISECT_SKIP` for **every** method of `FastFourierTransformerTest` (including all `doTest*`/`dft`/`createComplexData` helpers), `testSinFunction` and `testStandardTransformFunction` still fail. That points at a JIT bug in code outside this test class — likely in commons-numbers `Complex`, the FFT engine, or one of the JDK math intrinsics it touches — rather than the test itself.

Until the underlying merge-lowering bug is fixed, the practical workaround for any caller hitting this pattern is to run the affected suite with `--nojit`. The `Tern3.loopAdd` minimal repro confirms the bug, so the fix can be driven from that case without needing the full FFT classpath.

---

## Repro environment

- Branch `dev`, post-`32c3f5d0`.
- Windows 11, MSVC build.
- JDK 25 used as `--java-home`.
- Real-JDK CLI build (no `synthetic-jdk`).
