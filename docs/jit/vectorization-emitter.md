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

Three further obligations the emitter cannot check, stated on the module doc:
every array base is **non-null** (the region dereferences it unchecked; the
`LengthAtLeast` binding already required its length to be loaded); `bound` is
the **exclusive, increasing** limit (an `i <= n` loop passes `n + 1`, computed
in 64 bits); and the subscripts are `iv + offset` at unit scale and `+1` step,
which since round 9 is the only shape `vector_gate` admits.

## Shapes that are emitted

| element | element-wise | reduction |
|---|---|---|
| `int` | `+ - * & \| ^` and `min`/`max` (`*`, `min`, `max` only when the plan's ISA claims SSE4.1) | `+ \| ^ &` |
| `long` | `+ - & \| ^` (no `min`/`max`: `VPMINSQ`/`VPMAXSQ` are AVX-512) | `+ \| ^ &` |
| `float` | `+ - * /` | — |
| `double` | `+ - * /` | — |
| `byte` / `char` / `short` | copies only — a body of `Load`/`Store` steps and nothing else | — |
| reference | — | — |

Everything not in that table is a refusal, not a fallback to something slower.

The body is handed over as a straight-line single-assignment list of
`VecStep::{Load, Binary, Store, Accumulate}`. Each value must be defined exactly
once and used at least once; a dead vector load is a producer bug and is refused
rather than emitted.

## The emitted layout

```text
  <guards>                       ; cmp + jcc → fallback  (caller patches rel32)
  <accumulator init>             ; reduction only: VPXOR / VPCMPEQD acc,acc,acc
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

`int` and `long` reductions with `+`, `|`, `^` and `&`. All four are associative
*and* commutative over the whole two's-complement domain at **both** widths — JVM
`int` and `long` arithmetic are equally modular, so this is the same argument
twice and not a weaker one at 64 bits — and each has an identity this module can
put in a register with one *self-referential* instruction, which is what makes
the accumulator init free of a constant pool:

| operator | identity | init |
|---|---|---|
| `+` `\|` `^` | `0` | `VPXOR acc, acc, acc` (`VEX.66.0F EF /r`) |
| `&` | all-ones | `VPCMPEQD acc, acc, acc` (`VEX.66.0F 76 /r`) |

The accumulator is initialised to that identity before the head test and folded
into the incoming scalar accumulator at the end. `VPCMPEQD`'s dword lane width is
irrelevant here — all-ones dwords are all-ones qwords — so the one row serves
both accumulator widths.

**Which operators are admitted is decided by two tables read together**,
`scalar_fold_opcode` (the final GPR fold) and `accumulator_identity` (the
register init), and `reduction_enc` requires *both* to answer. That pairing is
not redundancy: a fold row without an identity row is silent wrong code rather
than a refusal — an `&` reduction over a `VPXOR`-zeroed accumulator answers `0`
for every loop, on a path no `+`-reducing test can see. The two tables are pinned
against each other over the whole `VecOp` vocabulary by
`the_two_reduction_tables_admit_exactly_the_same_operators`.

The fold has TWO shapes, and the difference is the part that would be a bug if it
were implemented from the 32-bit listing alone. The 64-bit fold uses ONE
`VPSHUFD 0x4E` (`[2,3,0,1]` over dwords, which read as qwords is a half-swap),
`VMOVQ` (`VEX.128.66.0F.W1 7E /r` — note `W1`) and a `REX.W` ALU op. It must
**not** run `MOVSXD`: a `REX.W` op writes all 64 bits, so the value is already
exact, and re-extending it from bit 31 would corrupt every `long` sum outside the
`i32` range — which is most of the values a `long` accumulator exists for. It must
also not run the second `VPSHUFD 0xB1`, which at 64-bit lanes swaps the two halves
*inside* each qword and folds that back in, reducing nothing. Both absences are
asserted by `a_long_sum_reduction_folds_in_64_bits_and_never_re_extends`.

```text
  ; 32-bit lanes
  vextracti128 tmp, acc, 1          ; 256-bit only
  vp{add,or,xor,and}d acc, acc, tmp ; 128-bit form
  vpshufd tmp, acc, 0x4E
  vp{...} acc, acc, tmp
  vpshufd tmp, acc, 0xB1
  vp{...} acc, acc, tmp
  vmovd   scratch32, acc
  {add,or,xor,and} acc_gpr32, scratch32
  movsxd  acc_gpr, acc_gpr32        ; the 32-bit op zero-extended it

  ; 64-bit lanes
  vextracti128 tmp, acc, 1          ; 256-bit only
  vp{add,or,xor,and}q acc, acc, tmp ; 128-bit form
  vpshufd tmp, acc, 0x4E            ; ONE shuffle — see above
  vp{...} acc, acc, tmp
  vmovq   scratch64, acc            ; VEX.128.66.0F.W1 7E /r — note W1
  REX.W {add,or,xor,and} acc_gpr, scratch64
                                    ; and NO movsxd
```

`VPAND` is `VEX.66.0F DB /r` in both arms of `arith_opcode`, so the horizontal
tree for `&` is the `+` tree with one opcode byte changed; the scalar fold is
`and r/m, r` = `21 /r`, the same byte at both widths under `REX.W`.

The final `MOVSXD` (round 9) restores the sign-extended `int` representation
the shape contract promises. Without it a negative sum came back
zero-extended — correct in the low 32 bits, but a large positive number to any
64-bit consumer (`i2l`, a 64-bit compare, a spill reloaded whole).

Because the vector accumulator starts at the identity, the epilogue is also
correct when the head test fails on its first evaluation: zero vector passes fold
an identity in, which is a no-op. For `&` that is all-ones `and`-ed into the
scalar accumulator, which leaves the low 32 bits alone and is then re-extended by
`MOVSXD` exactly as for `+`. It is asserted by
`the_and_identity_is_in_place_before_the_head_test_can_skip_every_pass`, and it
is the case a `VPXOR`-zeroed `&` accumulator would break most visibly: a loop
that ran no vector code at all would still destroy a correct scalar result.

Refused:

* `*` — identity is `1`, and unlike all-ones it is not one instruction. The
  cheapest sequence is `VPCMPEQD` then `VPSRLD acc, acc, 31`, and
  `VPSRLD xmm, xmm, imm8` is `VEX.NDD.128.66.0F 72 /2 ib`: the destination is in
  `vvvv`, the source in `r/m`, the opcode extension in ModRM `reg`. That is a
  different operand layout from every form `Asm` has (`vec_rr` puts the
  destination in ModRM `reg`), so it is a new encoder family rather than a table
  row — structurally the same obstacle as the `VMOVQ` load/store pair that keeps
  `emitter_encodes_width` at `{16, 32}`. `long *` could not follow in any case:
  `VPMULLQ` is AVX-512.
* `-`, `min`, `max`, `/`, `%` — no identity, or no associativity, or both.
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

Sub-word elements (`byte`, `char`, `short`) are refused for a different
reason. JVM arithmetic on them is performed in `int` (on sign-extended `byte`/
`short` and zero-extended `char` operands) and narrowed only at the store. For
`+ - * & | ^` whose result goes *straight* to a same-width store, a lane-wise
`PADDB`/`PADDW` is actually exact — the low bits of those operations depend only
on the low bits of their operands. It stops being exact the moment the `int`
value is used any other way: a shift right, a compare, a divide, an `int`
reduction (`sum += b[i]`), a store into a wider array. `VecStep` cannot say
"this value is only ever narrowed", so sub-word bodies that **compute** are
refused.

A sub-word **copy** is emitted. `b[i] = a[i]` over `byte[]`/`char[]`/`short[]` has
no arithmetic node at all, and `VMOVDQU` has no lane width — `move_opcodes`
returns the same `(pp = 2, 0x6F, 0x7F)` row for 1-, 2-, 4- and 8-byte elements —
so the only element-dependent part of the address is the SIB scale, which
`scale_log2` already answered for 1 and 2. The refusal's condition is a scan of
the handed-over `shape.body` for a `Binary` or `Accumulate` step, deliberately
**not** a deduction from the gate's `MixedElementWidths` (which depends on the
producer labelling an `int`-width arithmetic node `Int` — a convention, not a
derived fact), and `arith_opcode`'s sub-word arm is an unconditional second
refusal, so a body scan widened by a later edit still cannot reach a lane-wise
sub-word opcode.

Adding the missing fact (a `NarrowedOnly` value class, plus `VPMOVSX`/`VPMOVZX`
widening loads for the reduction case) is still the cheapest route to sub-word
*arithmetic* — but no longer to `byte[]`/`char[]` coverage as such, which the copy
case already took for `System.arraycopy`-shaped loops without any of it.

## What the gate admits and this emitter refuses

Kept as a list on purpose. `vector_gate` and this module are two files that must
agree, and nothing mechanical ties them together — so between round 10 waves 5
and 9 the same shape, *the gate admits a plan the emitter always refuses*, was
rediscovered one page at a time, each time by a reader holding both files in
their head. The list is cheaper than the next rediscovery. It is what is left of
`docs/internal/retired/r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode-20260921-RETIRED-20260922.md`,
which closed once four of its five classes were encoded (`long` reductions,
sub-word copies and `int` `min`/`max` in wave 8, `&` reductions in wave 9).

None of these is a wrong-code risk: every one is a hard refusal, and the caller's
untouched scalar loop is the fallback.

| shape | emitter's answer | why it stays refused |
|---|---|---|
| `double s += a[i]` under `FpRelaxation::AllowReassociation` | `UnsupportedReduction { Double, Add }` | **Deliberate, permanently.** The relaxation is the caller saying it accepts a different FP result; this module does not take the caller's word for one. Two independent gates on the one transform here whose answer is not bit-identical to the scalar loop. |
| `int s *= a[i]` | `UnsupportedReduction { Int, Mul }` | identity `1` needs an operand layout `Asm` does not have — see *The reduction epilogue*. `long *` additionally has no lane instruction outside AVX-512 (`VPMULLQ`). |
| `c[i] = Math.max(a[i], b[i])` over `long[]` | `UnsupportedOp { Long, Max }` | `VPMINSQ`/`VPMAXSQ` are AVX-512. The gate cannot express this: `int32_mul_minmax` is a 32-bit claim by name and its use is guarded by `elem_bytes == 4`, so this refusal is the only thing saying no — and it explains itself, which is why it is left as the only thing. Adding an `int64_minmax` field that is `false` on every target this tree models would buy one earlier refusal and a fourth place where an AVX-512 boundary is modelled; do it in the commit that adds a target setting it `true`. |
| sub-word **arithmetic** | `SubwordElement { Byte }` | `VecStep` cannot say "this value is only ever narrowed"; see *The oop-store refusal*. The sub-word **copy** is emitted. |

Not on the list, for completeness: `VecOp::Neg` is in the gate's vocabulary and
`VecStep` has no unary form, so a producer cannot build the shape to be refused —
a vocabulary mismatch between two of this module's own types, not an admitted
plan. And `VecEmitRefusal::UnprovableAlignment` is unreachable from a
gate-produced plan in the *opposite* direction: `admit_vectorization` already
pushes `VecRefusal::UnprovableAlignment` for exactly the `NaturalRequired` +
`Unknown` pair, so a plan arriving here with both can only have been hand-built.
Both arms are hard refusals and both are covered; it is a taxonomy redundancy and
not a defect.

The structural fix, when the call site in `jit/src/x64/driver.rs` lands, is for
the caller that owns both ends to check the gate's admission against the
emitter's capabilities once — so a loop of one of these shapes is declined before
it pays for a dependence test, `scev` index proofs, guard construction, an
alignment verdict and a trip-count witness, on every compile, forever.

## CPU features

Every instruction emitted is VEX-encoded, so `emit_vector_loop` refuses unless
`HostVectorSupport::detect()` (which reads `cpu_features::has_avx2`) reports
AVX2. Emitting AVX2 on a machine without it is a `SIGILL`, so the host fact and
the plan's modelled `VectorIsa` are checked separately and both must agree:

* host must have AVX2;
* the plan's ISA must be an `AlignmentPolicy::UnalignedOk` one (a
  natural-alignment target is a different emitter, not a different flag);
* `plan.width_bytes` must be 16 or 32 and equal `lanes * elem_bytes(elem)`;
* `int` `*`, `min` and `max` additionally require the plan's ISA to claim
  `int32_mul_minmax` (`PMULLD`, `PMINSD` and `PMAXSD` are all SSE4.1 — which is
  what that flag's name has always said), so a plan admitted against plain SSE2
  cannot reach any of the three.

Outside `cfg(test)` the only constructor of `HostVectorSupport` is `detect()`, so
production code cannot fabricate a capability.

## Displacements

Element addresses are `[base + iv*scale + (ARRAY_DATA_OFFSET + offset*elem_bytes)]`,
always with a SIB byte. `ARRAY_DATA_OFFSET` rather than `HEADER_SIZE` since
round 9: the two are equal today, and the planned array-length prefix separates
them (array data at 24 on a 16-byte object header). The displacement goes through
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

The flag is declared in `types/src/flag_groups.rs` (token `vectorize`), so
`CRATONVM_JIT=vectorize` works as well as the raw variable. Since round 9 the
same switch also admits the extra single-pass int-array-sum body forms (see
[The live vector paths](#the-live-vector-paths-x64simdrs)) — one opt-in for
"vectorize more", rather than a second undeclared knob.

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
* **Alignment is always `Unknown` on this VM.** `PROVEN_OBJECT_ALIGNMENT` is 8,
  so element 0 of an array is not provably 16- or 32-byte aligned whatever
  `ARRAY_DATA_OFFSET` is. That is harmless here (every move emitted is the
  unaligned form) but means the aligned-move fast path is unreachable and
  untested.

## The live vector paths: `x64/simd.rs`

Everything above describes an emitter nothing calls. The vector code that
*does* run today is the single-pass backend's pattern pre-headers in
`jit/src/x64/simd.rs`, admitted by bytecode detectors in
`jit/src/x64/simd_analysis.rs` and by the driver's SIMD coverage gate
(`driver.rs`, `simd_covered`: every array touched must have `bound <=
length` and `iv >= 0` proven statically or by a surviving speculative-BCE
guard). Each pre-header runs once on loop entry, is poll-free and therefore
capped at `MAX_BULK_BYTE_LOOP_SPAN` (2^20) iterations, and leaves the original
scalar loop to finish.

| shape (bytecode) | emitter | notes |
|---|---|---|
| `long s = a[i] + s` over `int[]` | `emit_simd_int_array_sum` | AVX2, 64-bit lanes (`VPMOVSXDQ`), exact |
| `long s += a[i]`, `int s += a[i]`, `int s = a[i] + s` | same | **only under `CRATONVM_JIT_VECTORIZE` (`CRATONVM_JIT=vectorize`)** — round 9, default off |
| `out[i] = a[i] OP b[i]`, `int[]`, OP in `+ - * & \| ^` | `emit_simd_int_array_element_wise` | AVX2, emission-gated on `has_avx2()` |
| `sum += a[row][k] * b[k][col]` | `emit_matrix_dot_preheader` | scalar, 8-way unrolled, guarded |
| `byte[]` zero fill, stride store, sieve | `emit_bulk_*`, `emit_byte_sieve_preheader` | `REP STOSB` / SWAR |

Round-9 fixes on these paths: the sum's vector pointer was formed with a 32-bit
`SHL EAX,2` (wrong address for a start index `>= 2^30`); the `int`-accumulator
arm now re-sign-extends its frame slot; the matrix-dot pre-header gained the
same poll-free span cap as its siblings.

**Coverage that is still refused** (see the round-9 lane notes for the ranked
list): any loop whose bound is `a.length` read in the header (`iload i; aload a;
arraylength; if_icmpge` — the form `javac` emits for `i < a.length`); every
`byte`/`char`/`short`/`long`/`float`/`double` element type; array fill/copy
(`a[i] = c`, `b[i] = a[i]`) outside the byte zero-fill; compare/search loops;
and anything with a scalar invariant operand (`a[i] = b[i] * k`).
