# The scratch-slot home also broke sixteen single-byte charsets

**Status: FIXED on `dev` 2026-09-02 by `c9b4a7d18`, which reverted the
mechanism.** This page is not that fix. It records a **second, independent
symptom** of the same mechanism, found from the other end and by a different
lane, and it is kept because the two symptoms together are what make the
revert's constraint legible: whatever touches `StackSlot::Scratch` must not
move the spill cursor, because the OSR entry's local homes derive from the same
frame layout.

> **The "underlying frame-growth defect" this page was written to preserve does
> not exist.** This page originally said `c9b4a7d18` left it "explicitly OPEN"
> and that whoever re-lands the optimisation needs both symptoms. There is
> nothing to re-land. The premise — that `flush_scratch_registers` reserves a
> fresh word per flushed value, so a call-heavy stretch grows the spill region
> once per call — was measured on 2026-09-02 on exactly that shape (one method,
> 64 sequential calls, all 64 results live across every later one) and reads
> `flush-calls=144 flush-reserved=1 peak-words=8 res-push=451 exhausted=0`,
> unchanged under `CRATONVM_JIT_SPILL_SLOTS_CAP` at 16 and 8. There are only
> two scratch registers, so at most two values ever need a flush word. A
> canonical-home variant was built, shipped behind a kill switch, and withdrawn
> with its engagement counter reading zero in every arm.
>
> This residual has been taken on twice on the strength of a comment rather
> than a number. Read `spill_cursor_counts()` first.

`apps/probes/SingleByteCharsets` is the regression probe for this half. It is
green on `a9acf3ec2` and red on `8b84cd347`.

## The symptom, which is not a stress test

```text
sun/nio/cs/MS1251$Holder.<clinit>
  ExceptionInInitializerError
    <- ArrayIndexOutOfBoundsException: Index 1318 out of bounds for length 1280
       at sun/nio/cs/SingleByte.initC2B (SingleByte.java:367)
```

On a host whose default charset is windows-1251 — an ordinary Russian-locale
Windows box — that is reached by a bare `"...".getBytes()` with no explicit
charset. `c9b4a7d18` found the mechanism through `RMapGcStress`, a GC-stress
test; this is the same mechanism reachable from a one-line Java program.

It was **sixteen of twenty** charsets, each overflowing its own `c2b` by exactly
one 0x100 block: every `windows-125x`, every `ISO-8859-x` except `-1`, `KOI8-R`,
`KOI8-U`, `IBM866` and `x-MacCyrillic`. Two rows did not fail and both are
load-bearing controls the probe keeps:

* **`ISO-8859-1`** — the one table with no unmappable entry, so it needs exactly
  as many blocks as it allocates. A sweep of only the broken charsets would have
  read 100% failure and said nothing about which property mattered.
* **`UTF-8`** — not a `SingleByte` charset at all: the negative control for "did
  the whole charset subsystem break, or just this table?"

## The mechanism, from this end

`push_from_rax`'s scratch path pushed `StackSlot::Scratch(reg, home)` and
reserved `home` at PUSH time. `reset_spills` — which runs at **every instruction
boundary** — recomputed the next free word by scanning the operand stack for
`StackSlot::Frame` entries alone. A live `Scratch` owns a word and is not a
`Frame`, so with only scratch entries live the cursor rewound to
`base_spill_offset`, onto a word still spoken for:

```text
caload      -> Scratch(R8, base+0)   cursor base+8
<boundary>  -> reset_spills(): no Frame entries -> cursor base+0
ldc 65533   -> Scratch(R9, base+0)   SAME home
if_icmpne   -> flush stores BOTH to base+0; the second clobbers the first
```

The emitted comparison is the constant against itself:

```asm
2f3: movzx eax,word [rax+rcx*2+10h]   ; caload c2bIndex[index]
2f8: mov   r8,rax                     ; LHS
2fb: mov   rax,0FFFDh                 ; RHS
302: mov   r9,rax
305: mov   [rbp-60h],r8               ;  ─┐ one word,
309: mov   [rbp-60h],r9               ;  ─┘ two operands
313: cmp   eax,ecx                    ; 65533 == 65533 -> ALWAYS EQUAL
```

`initC2B`'s block-allocation gate is exactly that shape —
`if (c2bIndex[index] == UNMAPPABLE_ENCODING) { c2bIndex[index] = (char) off; off += 0x100; }`
— so it allocated a fresh block per character and walked off the end of `c2b` at
the sixth. The sibling comparison three instructions earlier
(`c == UNMAPPABLE_DECODING`) is correct, and that is the tell that mattered: its
left-hand side is a register-homed LOCAL, so it is `CalleeSaved`, which reserves
no word and cannot collide.

**This is a different failure path from the one `c9b4a7d18` documents.** That
one is "the OSR entry's local homes are derived from the same frame layout, so
moving the cursor moves what an OSR transition loads". This one needs no OSR
transition at all — two operands simply share a word inside one compiled body.
A re-land has to answer both.

## The constraint a re-land has to respect, stated from this symptom

If a slot kind owns a frame word, **every** cursor computation is a caller that
has to be found. `reset_spills` and `pop_stack`'s reclaim scan both matched
`StackSlot::Frame` alone, and matching on `Frame` reads as exhaustive when it is
not. The same shape has now bitten three times: see
`pop_does_not_reclaim_a_slot_a_buried_entry_still_owns` (Spring Framework's
`InvocableHandlerMethodKotlinTests.genericParameter`) and
`a_splice_does_not_rewind_the_cursor_under_a_buried_operand` (bc-java `LEATest`,
`ArrayIndexOutOfBoundsException: Index -1007687205`) in `jit/src/x64/tests.rs`;
both were `invalidate_callee_saved` repointing entries, this was `Scratch`
reserving one.

## Levers, for whoever picks this up

Measured on `8b84cd347`, where the defect was live:

```text
--nojit                                 PASSES
CRATONVM_JIT_KERNEL_REG_LOCALS=0        PASSES   <- the same lever c9b4a7d18 bisected to
CRATONVM_TIER_OSR_BACKEDGE=1000000000   PASSES
CRATONVM_JIT_THRESHOLD=1000000000       FAILS    <- the callee is compiled through the
                                                    OSR-driven eager direct-bind door,
                                                    not the counter door
```

That last row is the one worth keeping: the obvious tier lever does not engage,
because `[osr-bind]` compiles direct-call callees eagerly at the optimizing tier
regardless of their own invocation count.

## Two leads that did not survive measurement

Recorded so they are not re-followed.

* **"The `b2c` decode table must differ from the JDK's."** The first hypothesis,
  and it named a plausible mechanism with no measurement behind it. Refuted in
  one command: copying the JDK's own `b2cTable` literal into a local class
  prints byte-identical output on both VMs — same 256 chars, same 4 distinct
  high bytes, same 1024 bytes of `c2b` needed against a 1280-byte array. The
  data was never wrong.
* **`wide iinc`.** `off += 0x100` compiles to the WIDE form, because 256 does
  not fit a signed byte, and `wide` has a history here (an unimplemented
  `iinc_w` once left netty's `FastLz.compress` interpreted at 138x). Not causal:
  `off += 100` (narrow `iinc`) and `off = off + 0x100` (no `iinc` at all) fail
  identically. It did surface something real and latent, fixed alongside this
  page and labelled there as not the cause — `jit/src/ir.rs`'s two bytecode
  pre-scan walkers had no `0xc4` arm, and three length-table comments still
  claimed "currently latent — `jit_scan` rejects `wide`", which stopped being
  true when the widened forms were implemented.
