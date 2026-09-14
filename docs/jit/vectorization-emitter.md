# The vector emitter

`jit/src/x64/vec_emit.rs`

This is the second half of the vectorization work. The first half,
`jit/src/x64/simd_analysis.rs`'s `vector_gate`, decides whether a counted loop
*may* be widened and deliberately emits nothing. This module takes an admitted
`VecPlan` plus a concrete machine-level description of the loop and produces
bytes — or refuses.

It is **off by default** and **not wired into `x64.rs`**. Nothing calls it yet.

## Why it is this conservative

The deep-research report's P2 is an ordering claim: vector work before alias,
range, alignment, safepoint and deopt metadata are sound multiplies wrong-code
risk. Those dependencies now exist and the gate consumes them, but this VM has
shipped several silent heap-corruption bugs from emitters that guessed. So the
rule throughout is: **refusing to vectorize a loop is always better than
emitting machine code that is silently wrong.** Every refusal here is hard.

## Entry point

```rust
pub(crate) fn emit_vector_loop(
    req: &VecEmitRequest<'_>,
) -> Result<VecLoopCode, VecEmitRefusal>
```

```rust
pub(crate) struct VecEmitRequest<'a> {
    pub plan:   &'a VecPlan,             // from vector_gate::admit_vectorization
    pub shape:  &'a VecLoopShape,        // where the values live, what the body does
    pub guards: &'a [VecGuardValues],    // exactly one per plan.guards, in order
    pub host:   HostVectorSupport,       // HostVectorSupport::detect()
    pub policy: VecEmitPolicy,           // VecEmitPolicy::from_flags()
}
```

The caller's obligations on success are stated on the function's doc comment and
repeated here because getting (2) wrong is the whole hazard:

1. Place `code` immediately after the loop's pre-header, with `shape.iv`,
   `shape.bound` and every array base already live in the named registers.
   `iv` and `bound` are **64-bit registers holding sign-extended `int`s** — the
   head test is computed in 64 bits so `iv + lanes` cannot wrap the way the
   scalar `int` expression could.
2. Patch **every** offset in `fallback_sites` to the scalar loop's pre-header.
   They are `rel32` fields; a site at offset `s` needs `target - (s + 4)`.
3. Resume the *unmodified* scalar loop at `remainder_entry`, with `shape.iv`
   live.

## Shapes that are emitted

| element | element-wise | reduction |
|---|---|---|
| `int` | `+ - * & \| ^` (`*` only when the plan's ISA claims SSE4.1) | `+ \| ^` |
| `long` | `+ - & \| ^` | — |
| `float` | `+ - * /` | — |
| `double` | `+ - * /` | — |
| `byte` / `char` / `short` | — | — |
| reference | — | — |

Everything not in that table is a refusal, not a fallback to something slower.

The body is handed over as a straight-line single-assignment list of
`VecStep::{Load, Binary, Store, Accumulate}`. Each value must be defined exactly
once and used at least once; a dead vector load is a producer bug and is refused
rather than emitted.

## The emitted layout

```text
  <guards>                       ; cmp + jcc → fallback  (caller patches rel32)
  <accumulator init>             ; reduction only: VPXOR acc, acc, acc
loop_head:
  lea   scratch, [iv + lanes]
  cmp   scratch, bound
  jg    epilogue                 ; iv + lanes > bound → no full pass remains
  <body>
  add   iv, lanes
  jmp   loop_head
epilogue:
  <reduction fold, then fold into the scalar accumulator GPR>
  vzeroupper                     ; only when 256-bit registers were touched
  ; ← remainder entry
```

## Guards and the fallback edge

`vector_gate`'s module doc states the contract: *"a partially-emitted guard set
proves nothing"*. So `emit_vector_loop` requires one `VecGuardValues` binding per
`plan.guards` entry, in the same order, and refuses the whole request when the
counts differ or a binding does not fit its guard's shape.

The *comparison* is derived from the guard variant; the caller only says where
the runtime values are. A caller cannot supply a condition:

| `PreheaderGuard` | binding | emitted |
|---|---|---|
| `NonNegative(term)` | `Term { term }` | `cmp term, 0` + `jl` |
| `LengthAtLeast(term)` | `LengthAtLeast { length, term }` | `cmp length, term` + `jl` |
| `AtMost { limit, .. }` | `Term { term }` | `cmp term, limit` + `jg` |
| `AtLeast { limit, .. }` | `Term { term }` | `cmp term, limit` + `jl` |
| `TripCountAtLeast { minimum, .. }` | `Term { term }` | `cmp term, minimum` + `jl` |
| `StrideInRange { .. }` | — | **refused** |

All comparisons are signed and 64-bit, matching `scev`'s statement that these
terms are evaluated in 64 bits and must not be materialised as a wrapping `int`
add.

`StrideInRange` is refused because it exists only for a runtime stride
(`jit/src/scev.rs:1582` is its only producer, on the `Stride::Variable` arm), and
`admit_vectorization` already refuses a variable stride with
`VecRefusal::VariableStride`. It is unreachable today; refusing it costs nothing
and closes the case where that stops being true.

Guards are emitted **before any vector register is touched**. That is what makes
the fallback edges free of a `VZEROUPPER` obligation, and it is asserted by a
test that scans the bytes before the first fallback site for a VEX prefix.

## The remainder loop

There is no separate remainder *loop*. `TailStrategy::ScalarRemainder` is
implemented by falling out of the vector loop with the induction variable live in
its GPR, into the caller's **untouched** scalar loop, whose own header test then
runs the last `trip % lanes` iterations.

This is the only tail available: masking a partial vector needs AVX-512 predicate
registers, which `jit/src/x64/cpu_features.rs` does not detect and no `VectorIsa`
in `simd_analysis.rs` claims (`masked_tail` is `false` on every one of them).

`TailStrategy::None` emits identical code — the head test simply never fails
early. One shape for both means the tail strategy cannot be the thing that is
wrong.

## The reduction epilogue

Only `int` reductions with `+`, `|` and `^`. Those three are associative *and*
commutative over the whole two's-complement domain and have `0` as identity, so
the vector accumulator is zero-initialised with `VPXOR` and folded into the
incoming scalar accumulator at the end.

```text
  vextracti128 tmp, acc, 1      ; 256-bit only
  vp{add,or,xor}d acc, acc, tmp ; 128-bit form
  vpshufd tmp, acc, 0x4E
  vp{...}d acc, acc, tmp
  vpshufd tmp, acc, 0xB1
  vp{...}d acc, acc, tmp
  vmovd   scratch32, acc
  {add,or,xor} acc_gpr32, scratch32
```

Because the vector accumulator starts at the identity, the epilogue is also
correct when the head test fails on its first evaluation: zero vector passes fold
an identity in, which is a no-op.

Refused:

* `&` — identity is all-ones, which needs a materialised constant.
* `*` — identity is 1, same problem, and `VPMULLQ` is AVX-512.
* `long` — the horizontal fold would need `VMOVQ` and a different shuffle tree.
* `float` / `double` — refused **unconditionally**, even when the gate admitted
  the reduction under `FpRelaxation::AllowReassociation`. The emitter does not
  take the caller's word for a changed floating-point result.

## The oop-store refusal

A vector store of object references bypasses the GC write barrier. That is the
same barrier-elision family that produced a use-after-free on this branch, and
there is no lane count that makes it safe.

`vector_gate` already refuses it (`VecRefusal::GcReferenceAccess`). This module
refuses it **again and independently** on `plan.elem == MemKind::Ref`, and the
`move_opcodes` / `arith_opcode` tables refuse `MemKind::Ref` on their own so the
refusal survives someone bypassing the entry point. Two gates, because one of
them being edited away should not be enough to ship a barrier-free oop store.

Sub-word elements (`byte`, `char`, `short`) are refused for a different but
equally sharp reason: JVM arithmetic on them is performed in `int` and narrowed
only at the store, so a lane-wise `PADDB` wraps at 8 bits where the scalar loop
wraps at 32.

## CPU features

Every instruction emitted is VEX-encoded, so `emit_vector_loop` refuses unless
`HostVectorSupport::detect()` (which reads `cpu_features::has_avx2`) reports
AVX2. Emitting AVX2 on a machine without it is a `SIGILL`, so the host fact and
the plan's modelled `VectorIsa` are checked separately and both must agree:

* host must have AVX2;
* the plan's ISA must be an `AlignmentPolicy::UnalignedOk` one (a
  natural-alignment target is a different emitter, not a different flag);
* `plan.width_bytes` must be 16 or 32 and equal `lanes * elem_bytes(elem)`;
* `int` `*` additionally requires the plan's ISA to claim `int32_mul_minmax`
  (`PMULLD` is SSE4.1), so a plan admitted against plain SSE2 cannot reach it.

Outside `cfg(test)` the only constructor of `HostVectorSupport` is `detect()`, so
production code cannot fabricate a capability.

## Displacements

Element addresses are `[base + iv*scale + (HEADER_SIZE + offset*elem_bytes)]`,
always with a SIB byte. The displacement goes through
`jit/src/x64/disp.rs`'s `Disp::encode_for_base`, which picks the encoding, applies
the RBP/R13 `mod=00` rule, and **errors** rather than narrowing when the value
does not fit a signed 32-bit field. No `as u8` / `as i8` narrowing of a
displacement happens anywhere in this module; three such raw narrowings elsewhere
in the JIT were recently converted to checked ones for exactly this bug class.

## Registers

`jit/src/regalloc.rs` has no vector register *class*, but it
does have the **authority**: `regalloc::xmm_roles` declares all three XMM
ranges in one place — `ir_lower`'s FP scratch pair (XMM0/XMM1), its linear-scan
file (XMM2–XMM7) and the widest pool a vector region may be given. The pool is
an **argument**, `VecEmitRequest::vector_pool`, because which registers are free
is a fact about the surrounding method:

* **XMM8..XMM15** is the pool, and it is **disjoint** from both
  scalar ranges. A vector region can no longer destroy a scalar `double` a
  caller left live, so the "prove your scalars are dead" obligation is gone.
  What replaced it is narrower and mechanical: every register in the pool is
  non-volatile on Windows, so a caller must say which ones its own prologue
  saves (`VecEmitRequest::frame_saved_xmms`) and a pool it does not save
  refuses the whole region (`VecEmitRefusal::UnusableVectorPool`) rather than
  being quietly narrowed. On System V every XMM is volatile and the field is
  ignored.
* The pool is XMM8..XMM15 rather than the low half because `vec_emit` encodes
  with VEX, which carries the high register bit for free. The constraint that
  pins `IR_LINEAR_SCAN` to XMM0..XMM7 — `fp_load`/`fp_store`/`fp_binop` emit no
  REX — does not apply here.
* An **empty pool is legal** and refuses at the first allocation. That is the
  right answer for a caller that has done no analysis.
* Lowest-free-index allocation, freed at each value's last use.
* Binary steps free their dead sources *before* allocating the destination — the
  VEX three-operand form reads both sources before writing, so reusing a source's
  register for the result is safe.
* **No spilling.** Running out is `VecEmitRefusal::OutOfVectorRegisters`.
* `RSP` is refused in every GPR role: it has no SIB-index encoding and
  clobbering it destroys the frame.

`VecLoopCode` reports `clobbered_vector_regs` and `clobbered_gprs` so the call
site can reconcile with the scalar allocator when it is wired up.

## The switch

`VecEmitPolicy::from_flags()` reads `CRATONVM_JIT_VECTORIZE` and answers
`Disabled` unless it is set to something other than `""`, `0`, `false`, `off` or
`no`. `emit_vector_loop` refuses a `Disabled` request before looking at anything
else.

The flag is **not yet declared in `types/src/flag_groups.rs`**, so it currently
works only as a raw environment variable, not as `CRATONVM_JIT=vectorize`.
Adding the declaration is a one-line follow-up in a file this change did not own.

## What is not validated

Everything below is honestly untested, because nothing calls this module yet:

* **No emitted code has ever been executed.** The tests assert byte sequences
  against hand-derived encodings and against the AVX2 helpers already in
  `x64.rs`; none of them run the bytes. An end-to-end execution test needs the
  call site.
* ~~**Partial integration with the scalar register allocator.**~~ **CLOSED (second half).** The first half declared the three XMM authorities
  together in `regalloc::xmm_roles` and made the pool a caller-supplied
  argument. The residual was that the pool still overlapped both scalar ranges
  completely, so the caller's "these are dead" proof was real work — and it
  could not be fixed here, because the only registers that would separate them
  are callee-saved on Windows and `ir_lower::emit_prologue` saved nothing.

  That prerequisite landed: `ir_lower::IR_LOWER_SAVED_XMMS` is a callee-saved
  XMM save area, emitted in the prologue and restored at all three exits. The
  scalar file moved to XMM2..XMM7 and this pool moved to XMM8..XMM15, and
  `regalloc::xmm_roles::disjointness_violation` now returns `None`.
  `vec_emit::tests::the_three_xmm_authorities_are_disjoint` replaced the test
  that used to assert the overlap — which said in its own doc comment that it
  would fail the day a save area landed, and did.
* **No call site.** `x64.rs` does not call `emit_vector_loop`, and no producer
  builds a `VecLoopShape` from the IR. The gate's `VecPlan` is `NodeId`-based;
  something has to lower those nodes to registers and steps.
* **No safepoint inside the vector loop.** The gate refuses a safepoint in the
  body, but the vector loop as emitted has no back-edge poll either. A long
  vector loop therefore delays a safepoint by up to `trip / lanes` iterations
  relative to the scalar loop. That is a latency question, not a correctness
  one, but it is unmeasured.
* **No deopt metadata.** There is no way to describe "lane 3 of this vector was
  iteration 11", which is why the gate refuses deopt points in the body. The
  emitted region is not describable to the deoptimizer at all; the fallback edge
  and the scalar remainder are the only re-entry points.
* **No cost model.** Nothing decides whether widening a given loop is
  *profitable*, only whether it is *legal*. `TripCountAtLeast` is the guard shape
  a profitability threshold would use.
* **Alignment is always `Unknown` on this VM.** `PROVEN_OBJECT_ALIGNMENT` is 8
  and `HEADER_SIZE` is 32, so element 0 of an array is not provably 16- or
  32-byte aligned. That is harmless here (every move emitted is the unaligned
  form) but means the aligned-move fast path is unreachable and untested.
