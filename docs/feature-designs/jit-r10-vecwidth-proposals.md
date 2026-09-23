# Directions worth pursuing in `simd_analysis` / `vec_emit`, after the width fix

Written round 10 wave 6, lane `vecwidth`. Everything here comes from reading the
tree on 2026-09-21; this lane was not permitted to build or run anything, so no
number below was measured by it and none is presented as if it were. Where a
byte sequence is given it is from the Intel/ARM encodings, not from a
disassembly of anything this lane produced.

Companion to `jit-r10-vecplan-proposals.md` (wave 5), which this does not
repeat. Two of its items are **corrected** below rather than restated: its
proposal 2 (the width table, now fixed, and fixed differently from the shape it
suggested) and its proposal 6 item 2, which mis-ranks `long` reductions.

## What changed in wave 6, so these read against the current tree

`VectorIsa` gained `min_width_bytes` — the narrowest vector this tree has a
move for on that target, declared per target beside `int32_mul_minmax` and
`masked_tail`. `admit_vectorization` refuses below it
(`VecRefusal::WidthBelowIsaMinimum`) instead of publishing an 8-byte plan no
emitter takes. `vec_emit`'s width table became a named
`emitter_encodes_width`, so the two have something to be pinned against
(`the_narrowest_width_each_isa_declares_is_one_this_emitter_encodes`).

Everything below assumes that field exists, because every proposal that widens
an emitter has to lower it in the same commit or the widening is invisible to
the gate.

## Proposal 1 — the `VMOVQ` tier, and the one thing that makes it non-trivial

**Buys:** `int[]`/`float[]` loops whose tightest backward dependence distance is
2 or 3 — `a[i] = a[i-2] + b[i]` is the textbook shape — which are currently
refused outright by `WidthBelowIsaMinimum` and run scalar.

**Costs:** one new encoder family in `move_opcodes`, and a change to its
signature.

The pieces, checked against the code rather than assumed:

* The moves are `VMOVQ xmm, m64` = `VEX.128.F3.0F 7E /r` and `VMOVQ m64, xmm` =
  `VEX.128.66.0F D6 /r`. **They do not share a mandatory prefix.**
  `move_opcodes` returns `(pp, load_op, store_op)` — one `pp` for both
  directions — because every element type it serves today is symmetric
  (`VMOVDQU` `F3 6F`/`F3 7F`, `VMOVUPS` `10`/`11`, `VMOVUPD` `66 10`/`66 11`).
  So this is a return-type change to the helper every emitted load and store
  goes through, including the 16- and 32-byte paths that are byte-tested. Do
  that change first, on its own commit, with the existing byte tests unchanged
  as the check.
* `l` needs no third state. `let l = plan.width_bytes == 32` is already `false`
  for 8, which is the correct `VEX.L` for a half-register move.
* **The reduction fold needs no new arm**, contrary to the note in
  `r10-vecplan-dependence-capped-widths-…`. `VMOVQ xmm, m64` zeroes bits
  64..127, and the accumulator starts at `VPXOR`-zero, so lanes 2 and 3 of a
  two-lane `int` accumulation are zero for the life of the loop. The existing
  four-lane tree (`VPSHUFD 0x4E`, add, `VPSHUFD 0xB1`, add) already computes
  `a0 + a1`. It does one redundant shuffle, which is two bytes and a cycle in
  an epilogue that runs once.
* `scale_log2` and `element_disp` need nothing: the address is
  `base + iv*elem_size + disp` regardless of how many lanes the move touches.
* `min_width_bytes` drops to 8 on the four x86 targets in the same commit, and
  `emitter_encodes_width` gains 8. `WidthBelowIsaMinimum` then becomes
  reachable only for sub-word elements (a `byte[]` capped at distance 2 is a
  **2**-byte vector), which is the right residue.

Sequencing matters here. The `move_opcodes` signature change is invisible
(existing tests pin the existing bytes); the `VMOVQ` rows are new bytes nobody
has executed. Keep them separate so a bisect can tell them apart.

## Proposal 2 — sub-word **copies**, which need no `VecStep` bit at all

This is a strict subset of wave 5's proposal 6 item 1 and it is much cheaper
than that item implies, because of a fact that item did not use.

Item 1 says the missing piece is "one bit on `VecStep`: this value is only ever
narrowed". That is true for sub-word *arithmetic*. It is not needed for a
sub-word **copy** (`b[i] = a[i]` over `byte[]`/`char[]`/`short[]`), and the
reason is on the gate side: a sub-word body that does arithmetic is **already**
refused before the emitter ever sees it, by `VecRefusal::MixedElementWidths`.
The gate requires every `VecArith`'s `elem` to equal the accesses' `elem`, and
JVM arithmetic on `byte`/`char`/`short` is computed in `int`, so the arithmetic
node's `elem` is `Int` and the accesses' is `Byte` — unequal, refused. The gate
says so itself, in the comment on that check: "A sub-word element with an `int`
operation is the same refusal for the same reason."

So the only sub-word plan that reaches `emit_vector_loop` is one with no
arithmetic, and for that plan the lanes are moved bit for bit and a lane-wise
wrap cannot happen because nothing wraps.

What it takes:

* `move_opcodes` gains `MemKind::Byte | Char | Short => (2, 0x6F, 0x7F)` —
  the **same row** `Int` and `Long` already use, because `VMOVDQU` does not know
  its lane width. No new opcode byte, which is what makes this the cheapest
  item on this page after proposal 3.
* `scale_log2` already answers `Some(0)` for 1 byte and `Some(1)` for 2.
* The `SubwordElement` refusal narrows from "the element type is sub-word" to
  "the element type is sub-word **and** the body contains a `Binary` or
  `Accumulate` step". Keep the refusal variant and its `elem` payload; this is a
  narrowed condition, not a deleted check.
* Belt and braces worth having: `arith_opcode`'s sub-word arm stays an
  unconditional refusal, so even if the step check above is ever widened by
  accident, no sub-word lane-wise arithmetic can be encoded. Two independent
  gates, which is the pattern the module already uses for oop stores.

**Buys:** `System.arraycopy`-shaped `byte[]`/`char[]` loops, which are common in
string and I/O code, at 32 lanes per pass on AVX2.

**Risk to name honestly:** the narrowed condition is a body scan, and a body
scan that a later edit gets subtly wrong emits sub-word arithmetic. The
`arith_opcode` backstop is what makes that a refusal instead of wrong code, so
it is not optional.

## Proposal 3 — the `long` reduction fold, which is the cheapest item here

Wave 5's proposal 6 item 2 groups "`&` and `*` reductions, and `long`
reductions" under one obstacle — "the accumulator cannot be zero-initialised"
— and adds that "`VPMULLQ` is AVX-512". That is right about `&` and `*` and
**wrong about `long +`**, and the mis-grouping hides the easiest win in this
module:

* `&`'s identity is all-ones and `*`'s is one. Both need a materialised
  constant. Genuinely awkward, correctly ranked.
* `long +`'s identity is **zero**, exactly like `int +`'s. `VPXOR` initialises
  it correctly. And `VPADDQ` (`VEX.66.0F D4 /r`) is *already encoded* in
  `arith_opcode`'s `Long` arm, so the accumulate step inside the loop needs
  nothing new. `VPMULLQ` is about `long *`, which stays refused.

What actually blocks `long s += a[i]` is one line and one epilogue arm:

* `emit_vector_loop`'s `reduction_enc` opens `if plan.elem != MemKind::Int`.
* The epilogue's fold is 32-bit: two `VPSHUFD`s reduce four dwords to one,
  `VMOVD` moves 32 bits to a GPR, `scalar_fold_opcode` gives a 32-bit `add`
  (`0x01`), and `movsxd_self` then repairs the sign because a 32-bit ALU op
  zero-extends.

For two `long` lanes in an XMM the fold is *shorter*: one `VPSHUFD 0x4E` (which
swaps the two 64-bit halves when read as dwords — `0x4E` is `[2,3,0,1]`), then
`VPADDQ`, then `VMOVQ r64, xmm` (`VEX.128.66.0F.W1 7E /r` — note `W1`, and note
that this is the *other* `VMOVQ` direction from proposal 1) and a `REX.W`
scalar `add`. `movsxd_self` becomes unnecessary and must be **skipped**, not
kept: a 64-bit `add` writes the whole register and re-sign-extending from 32
bits would corrupt every accumulator above `i32::MAX`. That is the one trap in
this proposal and it is worth a test of its own.

**Buys:** `long` sums over `long[]`, at 4 lanes per pass on AVX2. There is a
measurable benchmark behind this one, unlike proposals 2 and 4.

## Proposal 4 — `VPMINSD` / `VPMAXSD`, two rows, and a refusal string that promises them today

`arith_opcode`'s `Int` arm ends at `VPMULLD`; `Min` and `Max` fall through to
`UnsupportedOp`. Meanwhile the *gate* pushes

```text
MissingIsaFeature("32-bit integer multiply / min / max (PMULLD, PMINSD, PMAXSD — SSE4.1)")
```

only when the target lacks the feature — so on every target the host probe can
offer after wave 5 (AVX2 only), the gate clears `Math.max` over `int[]` and the
emitter refuses it. The refusal string is true about the CPU and misleading
about this tree; a comment at that site now points at the known-issues page, but
the real fix is the two rows:

* `VPMINSD` = `VEX.66.0F38 39 /r`, `VPMAXSD` = `VEX.66.0F38 3D /r`. Map 2,
  `pp = 1`, `W0` — the same shape as the `VPMULLD` row directly above them
  (`(2, 1, 0x40)`), so they are two literal tuples plus two byte tests.
* Keep `long` min/max refused: `VPMINSQ`/`VPMAXSQ` are AVX-512. The gate cannot
  currently express that distinction — `int32_mul_minmax` is a 32-bit claim by
  name and `elem_bytes(a.elem) == 4` guards its use — so the emitter's
  `UnsupportedOp` on `Long` min/max stays the only thing saying no. That is
  fine; it is a refusal that explains itself.
* FP `min`/`max` must stay refused at the gate **unconditionally**, and nothing
  here touches that: `MINPS`/`MAXPS` return the second operand for NaN and for
  signed zeros, which is a wrong answer rather than a reordered one. The
  proposal is about integer lanes only.

## Proposal 5 — make the gate/emitter agreement mechanical instead of a reading exercise

This is the one that would stop the family in
`r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode-20260921-RETIRED-20260922.md`
from producing one more page per wave.

Right now the only thing tying the two ends together is a reader who holds both
files in their head. Wave 5 found one mismatch that way, wave 6 found another,
and there are five left. The gate already has a corpus — 30 loops, each with the
verdict it must get — and `emit_vector_loop` already has fixtures. What does not
exist is a test that pushes the corpus's **admitted** plans through the emitter.

Shape:

1. In `vec_emit`'s test module (which can reach `simd_analysis`'s private
   `vector_gate`, as it already does for `VectorIsa`), a table of
   `(plan-producing corpus entry, VecLoopShape, expected emitter answer)`.
2. For every corpus loop the gate admits, the emitter's answer must be either
   `Ok` or a refusal **on this list of known gaps**, named per entry.
3. A refusal not on the list fails the test. A gap that stops being a gap also
   fails it, which is what makes the list shrink instead of rot.

The cost is real: a `VecLoopShape` per corpus loop is hand-written register
assignment, and the corpus lives in `simd_analysis`'s test module, so either the
fixtures move somewhere both can see or the table is duplicated. It is still
cheaper than one lane-wave per instance, and it converts "a careful reader
noticed" into "the suite says".

Note the ordering constraint if this is built: it must be written **against the
current five gaps as expected answers**, not as `assert!(is_ok())`. A test that
demands emission for all thirty would fail immediately and be deleted.

## Proposal 6 — the orphan component, and why it cannot be rooted from inside these two files

Recorded because seven lanes in round 10 hit the "no production caller" defect
class and this module's instance of it is structurally different from the
others, which is worth knowing before someone tries the usual fix.

`VectorIsa::detect`, `VecEmitPolicy::from_flags`, `HostVectorSupport::detect`,
`admit_vectorization` and `emit_vector_loop` are not five independent orphans.
They are one connected component: the first two feed the fourth, which feeds the
fifth, which needs the third. And the component is orphaned at its **root**, not
at a leaf. A root has to be something that drives a compilation, and neither
`simd_analysis.rs` nor `vec_emit.rs` is one.

So the usual remedy — give the orphan a caller — cannot be applied here without
making things worse. Adding, say, a `VecEmitRequest::for_host()` that calls
`HostVectorSupport::detect()` and `VecEmitPolicy::from_flags()` would move the
zero one level up and make `rg` *stop* reporting it, converting a visible gap
into an invisible one. Round 10 wave 6 deliberately did not do that, and said so
in `vec_emit`'s module doc so the next lane does not.

What it did do is give `VecEmitPolicy::from_flags` its first caller of any kind
(`reading_the_flag_and_reading_the_value_agree`), which pins the two things an
edit gets wrong — the key `CRATONVM_JIT_VECTORIZE` and the access path
`cratonvm_types::flags::runtime_var_os` rather than `std::env::var`. That is the
same move wave 5 made for `HostVectorSupport::detect`. It does not make either
of them live.

The only real fix is the call site in `jit/src/x64/driver.rs`, which is wave 5's
proposal 1. Whoever writes it should take
`r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode-20260921-RETIRED-20260922.md`
with them: the five classes are loops that will be analysed in full and refused
at the last step, and the caller is the place that can decline them before the
dependence test and the `scev` proofs are paid for.

## Not a proposal — one thing that is safe and looks like it is not

`emit_vector_loop` never checks that `plan.isa` is an x86 ISA. A plan carrying
`VectorIsa::neon128()` (16 bytes, `UnalignedOk`, `int32_mul_minmax`) passes every
check and gets VEX bytes emitted for it. Read twice, this is **not** a defect
and should not be "fixed":

* The bytes' legality is decided by `req.host.has_avx2()`, a check on the
  **host**, which is the first thing the function does. It is not decided by
  what the plan claims.
* Everything the emitter reads out of `plan.isa` afterwards is used
  conservatively: `width_bytes` as an upper bound, `alignment` as a refusal
  trigger, `int32_mul_minmax` as permission to encode `VPMULLD`. A NEON plan is
  16-byte, unaligned-ok and mul-capable, so every one of those answers is a
  legal x86 answer too.

Recorded so the next reader does not spend the ten minutes, and so nobody adds
an `isa.name == "avx2"` check that would break the `sse41()`-named fixtures the
gate's ISA-conditional tests depend on.
