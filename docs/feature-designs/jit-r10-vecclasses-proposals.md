# Directions worth pursuing in `simd_analysis` / `vec_emit` / `platform`, after the three encoders

Written round 10 wave 8, lane `vecclasses`. Everything here comes from reading
the tree on 2026-09-22; this lane was not permitted to build or run anything, so
no number below was measured by it and none is presented as if it were. Where a
byte sequence is given it is from the Intel encoding rules and from
`Asm::vex_prefix`'s own inversion arithmetic, not from a disassembly of anything
this lane produced.

Third in a series. `jit-r10-vecplan-proposals.md` is wave 5,
`jit-r10-vecwidth-proposals.md` is wave 6. This does not repeat either, and it
**corrects one item of each**:

* wave 6's proposal 3 (the `long` fold) and proposal 4 (`VPMINSD`/`VPMAXSD`) are
  **done**, and both were right in every detail this lane could check.
* wave 6's proposal 2 (sub-word copies) is **done**, but its *safety argument* is
  corrected rather than adopted: it rested the boundary on the gate's
  `MixedElementWidths`, which depends on a producer convention. See
  `docs/internal/retired/r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode-20260921-RETIRED-20260922.md`,
  section "The one claim that did not hold".
* wave 5's proposal 6 item 1 said the sub-word case needs "one bit on `VecStep`".
  For arithmetic, still true. For a copy, it needed nothing.

## What changed in wave 8, so these read against the current tree

`arith_opcode`'s `Int` arm gained `VPMINSD`/`VPMAXSD` behind the existing
`int32_mul_minmax` claim. `move_opcodes` gained a `byte`/`char`/`short` row (the
same `VMOVDQU` tuple `Int`/`Long` use) and lost its wildcard arm, so a new
`MemKind` is now a compile error there rather than a silent `SubwordElement`.
`emit_vector_loop`'s sub-word refusal is conjoined with a scan of `shape.body` for
`Binary`/`Accumulate`. `reduction_enc` admits `Int | Long`, and the epilogue has a
64-bit arm (`VMOVQ`, one shuffle, `REX.W` fold, **no** `MOVSXD`). Two new `Asm`
helpers, `vmovq_to_gpr` and `alu_rr64`. `emitter_encodes_width` is **untouched** —
still `{16, 32}` — and no `VectorIsa` field changed, so nothing about the
gate/emitter width pairing moved.

## Proposal 1 — `&` reductions, and the ORDER that keeps them from being wrong code

> **LANDED 2026-09-22 (wave 9), and the sequencing argument below is what it was
> built from.** The three edits went in together. Edit 3 was spelled slightly
> differently from the text below, and better: rather than "require both tables to
> answer", `reduction_enc` matches on the *triple* `(element is integer,
> scalar_fold_opcode(op), accumulator_identity(op))` and takes only the
> all-present arm, so there is one place that decides and no way to add a table
> without revisiting it. The init is `accumulator_identity`'s return value carried
> in `reduction_enc`'s tuple to the allocation site, so the site no longer has a
> literal at all. `the_two_reduction_tables_admit_exactly_the_same_operators` pins
> the pair over all eleven `VecOp` variants — the anti-drift half the proposal
> asked for by implication and did not name.
>
> `*` stays refused, as recommended. Every byte here held:
> `VPCMPEQD ymm8, ymm8, ymm8` is `C4 41 3D 76 C0`, the `VPXOR` init's prefix with
> one opcode byte changed, and the folds are `41 21 C0` / `49 21 C0`.

**Buys:** `int s &= a[i]` and `long s &= a[i]`, at 8 and 4 lanes per pass on AVX2.
No benchmark behind it, which is why wave 6 ranked it "worth doing when a real
`&`-reduction shows up in a profile" and why this lane did not take it.

**Costs:** three small edits that must land *together*, and the reason to say so
is that landing two of the three is silent wrong code rather than a refusal.

The instruction is genuinely already available. `VPCMPEQD acc, acc, acc` is
`VEX.66.0F 76 /r` — map 1, `pp = 1`, opcode `0x76` — a three-operand register
form that `Asm::vec_rr` emits today with no new helper, and comparing a register
with itself is all-ones regardless of what was in it, so it needs no constant pool
and no memory operand. It is correct at both widths and for `long` too: all-ones
dwords are all-ones qwords. `VPCMPEQD ymm` is AVX2, which the host check already
requires.

The three edits:

1. `scalar_fold_opcode` gains `VecOp::And => Some(0x21)` (`and r/m, r`, the same
   opcode at both widths under `REX.W`).
2. The accumulator init stops being the literal `VPXOR` it is today and becomes a
   function of the operator — `Add | Or | Xor => VPXOR` (identity `0`),
   `And => VPCMPEQD` (identity all-ones).
3. `reduction_enc`'s admission test requires **both** tables to answer, not just
   `scalar_fold_opcode`.

**Why the order and the pairing are the whole proposal.** Today
`scalar_fold_opcode` is the *only* thing gating which reductions are emitted, and
the accumulator init is an unconditional `VPXOR` written inline at the allocation
site. Edit 1 on its own admits `&` reductions whose accumulator starts at zero,
and `0 & anything` is `0`, so every such loop answers zero and every test that
sums a positive array still passes because it does not reduce with `&`. That is
the one wrong-code path in this family, it takes one line to create, and it is
invisible to the existing suite.

Edit 3 is what makes it impossible: with the admission test reading both tables,
a missing init is a `UnsupportedReduction` refusal instead of a wrong answer. So
the sequencing is **3, then 2, then 1** — make the gate demand the init, add the
init, then admit the operator. In that order every intermediate state is a
refusal.

The zero-pass case is correct and worth a test of its own: with all-ones in the
accumulator and no vector pass, the epilogue folds all-ones into the scalar
accumulator with `and`, which is a no-op on the low 32 bits — and then `MOVSXD`
restores the sign exactly as it does for `+`, because the incoming accumulator was
a sign-extended `int`.

**`*` is a different matter and should stay refused.** Identity `1` is not one
instruction. The cheapest sequence is `VPCMPEQD acc, acc, acc` then
`VPSRLD acc, acc, 31`, and `VPSRLD xmm, xmm, imm8` is
`VEX.NDD.128.66.0F 72 /2 ib` — the destination is in `vvvv` and the source is in
`r/m`, with the opcode extension in the ModRM `reg` field. That is a **different
operand layout** from every form `Asm` has (`vec_rr` puts the destination in
ModRM `reg`), so it is a new encoder family and not a table row — structurally the
same obstacle as the `VMOVQ` load/store pair in wave 6's proposal 1. And `long *`
has no lane instruction at all outside AVX-512 (`VPMULLQ`), so the `Long` half
cannot follow even if the `Int` half does.

## Proposal 2 — the residue of class D, and the flag that would let the gate say it

`long` `min`/`max` is admitted by `admit_vectorization` on every target it models
and refused by `arith_opcode` with `UnsupportedOp { Long, Min | Max }`. That is
the *correct* place for it by this round's own criterion — the refusal names its
own reason and a reader of it learns the truth — so this is not a defect and the
proposal is only about whether the gate could do better.

It could not, today, and the reason is in a name. The gate's check is

```rust
if !fp && matches!(a.op, Mul | Min | Max) && elem_bytes(a.elem) == 4 && !cand.isa.int32_mul_minmax
```

`int32_mul_minmax` is a **32-bit** claim by name and its use is guarded by
`elem_bytes == 4`, so it has nothing to say about 8-byte lanes. Expressing the
`long` case at the gate needs a second capability field — `int64_minmax`, say —
which every target this tree models would set to `false`, because `VPMINSQ` is
AVX-512 and `masked_tail` already records that no `VectorIsa` here models AVX-512
at all. A capability flag that is `false` on every target is a field whose only
effect is to make one refusal fire earlier, and it would be the *fourth* place
(after `int32_mul_minmax` and `masked_tail`) where this tree models an AVX-512
boundary it cannot cross.

So: leave it. Add the field only in the commit that adds a target which sets it to
`true`. Recorded because "the gate should refuse this" is the obvious reading and
it is the wrong one.

## Proposal 3 — the reachability of `WidthBelowIsaMinimum` changed, and it improved

Not a proposal so much as a consequence worth writing down, because wave 6
predicted it and it has now happened.

Wave 6's proposal 1 said that if `VMOVQ` ever lands, `WidthBelowIsaMinimum` would
become "reachable only for sub-word elements (a `byte[]` capped at distance 2 is a
**2**-byte vector), which is the right residue". `VMOVQ` did not land, but
sub-word *copies* did, and the effect on that refusal is the same in miniature: a
`byte[]` copy whose tightest backward dependence distance is 2..15 now reaches the
width check and is refused as `WidthBelowIsaMinimum`, naming the dependence,
where before wave 8 it would have been refused as `SubwordElement`, naming the
element type. The second answer was true and misleading — the element type was
never the obstacle for a copy.

A `byte[]` copy at distance 16 or more is 16 lanes, 16 bytes, at or above the
floor, and emits as VEX.128. That is the case
`a_subword_copy_emits_because_a_copy_has_no_arithmetic`'s last block pins.

No action. Recorded so that a reader who sees `WidthBelowIsaMinimum` on a
`byte[]` loop does not read it as the gate being confused about element widths.

## Proposal 4 — `instruction_stream_barriers_issued` wants one line in `vm-cli`

**LANDED, and watched printing.** On an x86-64 host, from a release binary:
`[cratonvm] JIT instruction-stream barriers issued: n/a on this target — x86-64
publication needs no explicit barrier, so nothing counts one. Not a zero
reading.` `fn instruction_stream_barriers_issued` is no longer in
`scripts/baselines/orphan-instruments-allowlist.txt`, and the landed code uses an
explicit `match` with `eprintln!` rather than the `println!` sketched below, so
the text and the value cannot disagree. Both obligations this proposal ends on
were met. Left in place as the argument for the shape, and because it is now the
precedent a second named zero was decided by — see
`docs/known-issues/jit/r10-readers-ir-tier-bails-a-whole-method-for-an-array-instanceof-target-20260922.md`.

**This was the only item on this page with a clear right answer and an owner other
than this lane.**

`jit::platform::instruction_stream_barriers_issued` now returns `Option<u64>`:
`Some(n)` where the reader-side barrier mechanism exists, `None` on x86-64 where
it is compiled out entirely. It still has no production caller and is still a
frozen entry in `scripts/baselines/orphan-instruments-allowlist.txt`.

Round 10 wave 6 declined to wire it and gave a good reason: with a `u64` return,
the only honest wiring was "a reader that is `cfg`-gated to the targets that write
it, or an explicit `<not applicable on this target>` rather than `0`", and either
was an aarch64 bring-up judgement that lane had no basis to make. The `Option`
removes that: the judgement now lives in `platform.rs`, where the knowledge about
which architectures have incoherent I-caches already lives, and the caller only
has to print what it is handed.

The line, in `vm-cli/src/main.rs`'s method-stats dump, beside the other JIT
counters:

```rust
match cratonvm_jit::platform::instruction_stream_barriers_issued() {
    Some(n) => println!("[cratonvm] instruction-stream barriers issued: {n}"),
    None => println!(
        "[cratonvm] instruction-stream barriers issued: n/a \
         (this architecture's instruction stream is coherent)"
    ),
}
```

`vm-cli/src/main.rs` belongs to another lane this wave, which is why this is a
proposal and not a commit. Two things whoever takes it must do in the same change:

* Re-freeze the allowlist (`scripts/check-orphan-instruments.sh
  --update-allowlist`) and say in the commit that
  `fn instruction_stream_barriers_issued` was **removed** from it because it
  gained a reader. The gate will print a `NOTE:` about it and will not absorb the
  improvement on its own — `SLACK` is zero by design.
* Not print the `n/a` line on a target where it is `Some`, and not print a bare
  `0` on any target. The whole point is that the two are different statements.

## Proposal 5 — over-wide visibility in `simd_analysis.rs`, and why this lane left it

Same shape as item 7 of
`docs/internal/fixed-bugs/r10-readers-platform-dead-public-surface-RESOLVED-20260922.md`:
declarations whose visibility promises a reader that does not exist. Checked with
`rg -rnw` over the workspace on 2026-09-22; each is "references in
`jit/src/x64/simd_analysis.rs`, zero elsewhere":

| item | declared | used |
|---|---|---|
| `vector_gate::dependence_between` | `pub(crate)` | in-file + its own tests |
| `SimdLoopBound::walk_local` | `pub(crate)` | in-file (two call sites) |
| `extract_istore_local` | `pub(super)` | in-file (two call sites) |
| `extract_lload_local` | `pub(super)` | in-file (two call sites) |
| `extract_lstore_local` | `pub(super)` | in-file (one call site) |

Every one is genuinely used, by code in the same file that would read it
identically at `pub(self)`/private. So this is surface, not dead code, and the
defect is that a reader of `pub(super) fn extract_lstore_local` reasonably
concludes some other `x64` module parses `lstore` through it, and none does.

Not narrowed here for the same reason platform.rs's five were not: three other
lanes this wave hold files in `jit/src/x64/`, and five visibility narrowings in a
file two of those lanes also read is tidiness bought with a merge conflict in
somebody else's diff. It is a ten-minute change for a quiet wave.

Also worth knowing before anyone sweeps for this shape mechanically: none of the
five is visible to `scripts/check-orphan-instruments.sh`. Its `C2` rule needs a
zero-argument `pub fn` returning an integer-ish type whose body touches an atomic;
these take arguments, return `Option<usize>`/`usize` without an atomic in sight,
and are not `pub`. The gate censuses instruments, not surface, and says so.

## Not a proposal — one dead arm in `emit_vector_loop` that should stay dead

```rust
let scale = match scale_log2(elem_size) {
    Some(s) => s,
    None => return Err(VecEmitRefusal::SubwordElement { elem: plan.elem }),
};
```

`scale_log2` answers `Some` for 1, 2, 4 and 8, and `elem_bytes` returns exactly
one of those for all eight `MemKind` variants (`Ref` is refused three lines
above). So the `None` arm is unreachable, and its refusal would be the wrong one
if it ever were reached — a hypothetical 16-byte element is not "sub-word".

Left alone deliberately. It is a total-match arm on a helper that returns
`Option`, the alternative is an `expect`/`unwrap` in a module whose contract is
that it never panics in production, and the cost of the arm being wrong is a
misleading refusal on a case that cannot occur — which is strictly better than a
panic on the same case. Recorded so the next reader does not spend the five
minutes, and so that nobody "fixes" it into an `unwrap`.
