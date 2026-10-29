> **j27 wave 9 (lane W9RPL, 2026-10-29): triaged as PERFORMANCE only; out of scope for wave 9 (behaviour
> gaps only), left filed.** No wrong answer is possible: when a scalar-intrinsic call sits inside a
> spliced callee, the JDK method's own bytecode body is spliced in its place. Each family's JDK body is
> the reference implementation (`Integer`/`Long.compress`/`expand`, `numberOfLeadingZeros`,
> `Math.max`, ...), and the page itself records that facts stay right. The only loss is the PEXT/PDEP
> or arithmetic lowering (int row 18.9 ns vs HotSpot 0.56 ns per element). No code changed.

# A scalar-intrinsic call inside a spliced callee is spliced as its JDK body, not lowered (`Integer`/`Long.compress`/`expand`)

Status: OPEN (found by builder lane TRIB, j27 triage, 2026-10-28; Windows, release LTO-off build of the j27 tree)
Area: JIT optimizing tier, splice planner vs `ir::try_ir_scalar_intrinsic` (`jit/src/lib.rs` site planning near
`ir_scalar_intrinsic_sites`, `ir_spliced_scalar_row` / `ir_plan_spliced_scalar_intrinsics`)

## Evidence

Microbenchmark (scratch, not in the tree): `ci(int[] x, int[] m)` sums `Integer.compress(x[i], m[i]) ^
Integer.expand(x[i], m[i])` over 1024 elements, `cl` the same with `Long`; `ti()` / `tl()` call `ci` / `cl` 200
times and are timed (min of the last 100 of 300 reps, pinned to P-cores, 7 interleaved runs per arm).

| ns / element | HotSpot 25 | CratonVM default | `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND=1` |
|---|---|---|---|
| int | 0.56 | 18.9 | 18.6 |
| long | 0.56 | 63.7 | 1.64 |

The `long` row shows the PEXT/PDEP lowering works (39x) when the timed body calls `cl`. The `int` row does not
move. `CRATONVM_DBG_JITC=1 CRATONVM_DBG_IR_COMPILES=1` shows why:

```text
[ir] scalar-intrinsics TribCompressBench2.ci([I[I)I: 2 site(s) lowered as arithmetic     <- ci's own compile
[cratonvm-jitc] ir-splice-bytes TribCompressBench2.ci([I[I)I at pc=22 rank=Some(InLoop) bytes=551 .. in TribCompressBench2.ti()J
[ir] spliced 9 callee bodies into TribCompressBench2.ti()J
```

`ti` splices `ci`, and inside the spliced `ci` the `Integer.compress` / `expand` calls are spliced too, as their
JDK bytecode bodies. The scalar-intrinsic map covers only the ROOT method's own pcs, and the splice-scalar route
(`CRATONVM_JIT_IR_SPLICE_SCALAR_INTRINSIC`) acts only on a call that SURVIVES a splice. A call whose callee
has bytecode is spliced instead, so it never survives. Whether the timed body is `ti` with `ci` spliced or `ti`
calling `ci` varies between runs (one run of the first bench version printed `time long 2.1` against a usual 30),
so the gain depends on which one wins.

The same gap applies to every scalar family whose JDK method has a bytecode body (`Integer.numberOfLeadingZeros`,
`Math.max`, `Integer.reverse`, ...) when the method that calls it is itself spliced. Facts stay right, since the
JDK body is correct. Only the speed is lost.

## What would fix it

When the splice planner (nested level) reaches a statically bound call that `ir::try_ir_scalar_intrinsic` claims,
it should leave the call as a surviving call row instead of splicing the body, so that the existing
`ir_plan_spliced_scalar_intrinsics` pass lowers it. One predicate should cover both paths. Two things must be
handled:

- `ir_spliced_scalar_row` restricts the splice route to `int`-shaped descriptors (no `J`/`F`/`D`), because a
  splice is not gated by the method's long/fp admission. `Long.compress` inside a splice needs that gate read
  from the root method's admission instead.
- Measure with the bench above and with `J27IntrinsicRows` (facts), both modes. Gate it behind a default-ON
  switch, since it changes the splice plan of every caller of such a family.
