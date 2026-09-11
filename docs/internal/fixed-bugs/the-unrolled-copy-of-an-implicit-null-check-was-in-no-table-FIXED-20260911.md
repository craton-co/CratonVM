# The unrolled copy of an implicit null check was in no table

**2026-09-11.** A null receiver dereferenced in an unrolled copy of a loop body
killed the process instead of throwing `NullPointerException`. Default-ON, in
the single-pass backend, on a shape javac emits constantly.

```java
static int walk(N o) {
    int a = 0;
    for (int i = 0; i < 5; i++) { a += o.v; o = o.next; }
    return a;
}
```

Over a **three**-element list, so `o` goes null in the fourth iteration:

| arm | `-Dprobe.reps=20000 -Dprobe.len=3` |
|---|---|
| HotSpot (Temurin 25) | `sum=0 traps=20000` |
| default tiering | **SIGSEGV** |
| `CRATONVM_NO_IR=1` (single-pass only) | **SIGSEGV** |
| `CRATONVM_NO_IR=1 CRATONVM_DISABLE_UNROLL=1` | `sum=0 traps=20000` |
| `CRATONVM_JIT=force-c2` | `sum=0 traps=20000` |

```text
EXCEPTION_ACCESS_VIOLATION (0xC0000005)
Faulting access: read at address 0x000000000000000F
jit: faulting pc is inside compiled method UT3.walk(LUT3$N;)I
```

## What it was

`jit::implicit_null`. A `getfield` whose receiver cannot be proved non-null
emits **no test at all**: the dereference is allowed to fault, and the SIGSEGV
handler translates it into an NPE by looking the faulting PC up in a table. The
faulting instruction is the compact arm's `GC_FLAGS` read at `[RAX + 15]` —
which is the `0x0F` in the crash report, and the reason the address looked like
a plausible field offset and was not one.

The native unroller duplicates the loop body's **machine code**. Each copy
therefore holds that dereference at a different PC, and the duplicator registers
none of them: it snapshots and shifts `forward_patches`, `bounds_check_stubs`,
`exception_check_stubs`, `null_check_store_stubs`, `self_call_patches`,
`deopt_stubs`, `jump_table_patches`, `oop_maps` and the inline-cache slots, and
`implicit_null_sites` was not on the list.

**The bodies are byte-identical.** Disassembling the same method with and
without `CRATONVM_DISABLE_UNROLL` gives the same instructions in the same order;
only the table differs. That is why no assertion about emitted code could have
found this, and why the first hypothesis — a missing or elided null check — was
wrong in a way that reading the disassembly appeared to confirm.

It is also why the vector is a **crash** rather than a wrong answer, and so
invisible to every checksum diff in the suite: a process that dies reports
nothing at all.

## Why the duplicator did not know

The list it works from was built by the Task #60 sweep, whose own comment says
it "removes the previous allow-list — earlier unrollers shifted only
`bounds_check_stubs`". The implicit null check landed after that sweep
(2026-09-02) and added a ninth vector nobody went back to add.

## The fix

One snapshot and one shifted extend, in the same shape as the eight neighbours.
The recovery address shifts **only when it is itself inside the duplicated
span** — every site this emitter makes recovers at its own arm's guarded slow
path a few bytes further into the same body, but an out-of-line recovery would
not be duplicated, and sending a copy's fault to `recovery + shift` would resume
it in whatever happened to be there. That is the one failure this table can
produce that nothing downstream catches, so the condition is written rather than
assumed.

## The tests, and why there are two

* `every_unrolled_copy_of_an_implicit_null_check_is_registered` asks the table
  by RANGE — how many PCs inside *this* artifact are registered — rather than by
  a `implicit_null::counts()` delta, which on a concurrent test harness counts
  whatever a sibling test compiled at the same moment. Two arms: one site in the
  rolled body, four in the unrolled one. Without the fix it reads **1 of 4**.

  The rolled arm is not decoration. The body has two `getfield`s and only one
  implicit site — the second is correctly elided, because the first dereference
  proves local 0 non-null and `o` is not reassigned until after it. Pinning that
  is what makes `4 *` mean something.

  It has to go through `compile_with_param_slots`: the legacy `compile()`
  wrapper passes an empty `method_key`, and `receiver_is_trusted_oop` requires a
  non-empty one, so a fixture compiled the legacy way registers nothing at all
  and both arms read zero.

* `RJitUnrollImplicitNpe` (regression suite) is the executable half. A unit test
  can assert the table; only a running VM can assert that the fault is
  *translated*. The unfixed binary dies on it; the fixed one prints
  `PASS RJitUnrollImplicitNpe`.

## Green

2358 `cratonvm-jit` unit tests, 145 `ir_vs_singlepass` differential, all 16
targets. Regression suite **93/93** against HotSpot (92 + the new vector).
`CratonBench` `5000000003999999995 701408733 9592 173943680 1549999915000000
5000050000 68332206` and `CratonBenchC2` `2893201123071733440
-1727289071355132288 97968176938830464`, unchanged.

## Found by

Probing the optimizing tier's per-copy deopt frames
(`docs/internal/performance/c2-per-copy-deopt-frames-20260911.md`). The probe
written to exercise a C2 deopt out of copy 3 crashed in C1 instead, because
`ir_evidence::accept` priced the unrolled C2 body as not worth publishing and
handed the method back to the single-pass tier. Unrelated to that work, and
pre-existing on `dev`.
