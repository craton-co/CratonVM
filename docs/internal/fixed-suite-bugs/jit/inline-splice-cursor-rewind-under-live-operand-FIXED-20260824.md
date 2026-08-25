# FIXED — a splice rewound the caller's spill cursor under a live operand

## Status

**FIXED 2026-08-24** on `fix/bcjava-sha1-alias-and-lea-20260824`.

Found by the 53-class bc-java sweep of 2026-08-24, which left exactly two
CratonVM failures. This is one of them; the other is the SUN `MessageDigest`
alias table, on its own page.

| | before | after |
|---|---|---|
| `org.bouncycastle.crypto.test.AllTests` | `Tests run: 21, Failures: 1` | **`OK (21 tests)`**, 2269 s |
| the same 195 `SimpleTest`s in one JVM | `bad=1` | **`bad=0`** |
| `cargo test -p cratonvm-jit --lib` | 2107 passed | 2107 passed, 0 failed |

## The symptom

```text
BAD 193 -> LEA: Exception: java.lang.ArrayIndexOutOfBoundsException:
                Index -1007687205 out of bounds for length 6
    at org.bouncycastle.crypto.test.LEATest.testFromVectorFile
```

`-1007687205` is `0xC3EFE9DB`. That is `LEAEngine.DELTA[0]`, and `length 6` is
`NUMWORDS192`, which names the 192-bit arm:

```java
private void generate192RoundKeys(final int[] pWork) {
    for (int i = 0; i < theRounds; ++i) {
        final int myDelta = rol32(DELTA[i % NUMWORDS192], i);
        int j = 0;
        pWork[j] = rol32(pWork[j] + rol32(myDelta, j++), ROT1);
        ...
```

`rol32(x, 0)` is `(x << 0) | (x >>> 32)`, and Java masks int shift distances to
five bits, so `rol32(DELTA[0], 0) == DELTA[0]`. The wrong index is therefore
`myDelta` on the FIRST iteration, and it reached `iastore` where `j` belongs.

Every array index in that key schedule is masked (`i & MASK128`,
`i % NUMWORDS192`, `index & MASK256`), so a wild index cannot be arithmetic. It
is a clobbered slot, and the value names its own writer.

## The defect

`try_emit_inline_body` restores the caller's spill cursor from the LOWEST frame
offset among the arguments it popped:

```rust
let mut caller_post_pop_spill = caller_spill_pre_reserve;
for i in (0..callee_num_args).rev() {
    let slot = self.pop_stack();
    if let StackSlot::Frame(off) = slot {
        caller_post_pop_spill = caller_post_pop_spill.min(off);
    }
    ...
```

That is the caller's new top only while frame offsets are handed out in stack
ORDER, so that the popped arguments are the topmost slots. Two mechanisms break
that, and both are load-bearing elsewhere:

* **`flush_scratch_registers`**, which this function calls on its first line,
  gives every `Scratch`/`Xmm` operand a FRESH slot at the cursor whatever its
  depth;
* **`invalidate_callee_saved`** reserves ONE fresh slot at the top and repoints
  every stack entry aliasing a register-homed local at it — which is what an
  `iinc` on a register-homed loop counter does, every iteration.

With either fired, the `min` puts the cursor BELOW a slot the caller still
owns, and the next reservation hands that address out twice: to this splice's
return value (`next_spill_offset = caller_post_pop_spill; push_from_rax()`), or
to the NEXT splice's `callee_local_base` — and `rol32`'s local 0 is `myDelta`.

The bytecode is the whole story. `pWork[j] = rol32(pWork[j] + rol32(myDelta,
j++), ROT1)` pushes the array-store index `j` first, runs `iinc` on `j` in the
middle, and holds that index live across TWO `rol32` splices before `iastore`
consumes it.

**`pop_stack` already carries this guard.** It grew a live-slot scan for the
same collision one table over — a Kotlin/Spring miscompile where
`invalidate_callee_saved` repointed a buried entry and the pop rewound past it.
This is that guard on the path that bypasses `pop_stack`'s reclaim arm
entirely: `reserve_spill_slots` has already moved the cursor past the argument
slots by the time the arguments are popped, which is precisely why the splice
derives the cursor by hand.

## The fix

After the argument-pop loop, clamp the cursor to sit above every frame slot
still on the caller's simulated stack. In the monotonic case this is a no-op —
the deepest popped argument's offset already equals `max(live) + 8`.

## The A/B, in one binary

`CRATONVM_JIT=-inline-live-slot-clamp` restores the pre-fix rewind. Same
binary, same host, 195 `SimpleTest`s in one JVM:

| arm | clamp | result | `inline_live_slot_clamps` | wall |
|---|---|---|---|---|
| 1 | **ON** (shipping) | **`bad=0`** | **121** | 2211 s |
| 2 | OFF | `bad=1` | 0 | 2144 s |

Arm 2 reproduces the identical line — same index, same array length. The
engagement counter is printed by `jit-method-stats` beside every other compile
number, so a result that credits this guard has to say whether it engaged.

## It is the JIT, and it needs the suite

Three controls on the pre-fix binary, all on `dev` `e645a7349`:

| arm | result | wall |
|---|---|---|
| 195 tests, JIT on | **`bad=1`** | 1445 s |
| 195 tests, `--nojit` | `bad=0` | 3578 s |
| `LEATest` ALONE, JIT on | `bad=0` | 27 s |

`--nojit` clears it, so it is compiled code. `LEATest` alone clears it, so it
needs the 193 tests that run first — which is what makes
`generate192RoundKeys` hot enough to be compiled with `rol32` spliced into it.
Unlike the reference-processing defect fixed the day before, this one is NOT
collector-specific: an earlier run of the same class failed identically under
the default collector, `--XX:UseGc G1` and `--XX:UseGc Generational`.

## The regression test

`a_splice_does_not_rewind_the_cursor_under_a_buried_operand` builds the hazard
directly on the operand-stack simulation: push a `CalleeSaved` entry, push a
computed value below it, run `invalidate_callee_saved` (the `iinc`), push the
second argument, splice a two-argument static leaf.

**It fails on the pre-fix tree**, which is the only thing that makes it worth
having:

```text
the splice left the cursor at 48 with the buried operand still owning slot 48:
the next reservation gets that address a second time
```

It asserts on the CURSOR rather than on a computed answer deliberately. A
bytecode-level test cannot reach this: the `compile` test wrapper never
requests register homes for locals and never sets `kernel_operand_cache`, so
neither mechanism that breaks monotonicity can fire, and a bytecode reproducer
written at that layer passes on the pre-fix tree — which one did, before this
one replaced it.

## What is not claimed

**Not that every cursor rewind in the splicer is now guarded.** This is the one
`try_emit_inline_body` derives by hand from popped arguments. The `merge_base`
reservation and the `next_spill_offset = callee_local_base` bail paths restore
a cursor the splice itself set and are not touched.

**Not a bisect.** How long this has been live is unmeasured. It requires
register-homed locals, an inline site, and a live operand under the call, and
all three predate this branch.

**Not that 121 is a defect count.** It is how many splices the clamp moved the
cursor for in one run — the number of times the guard had something to do, not
the number of miscompiles it prevented. Most of those splices would have
rewound under a slot nothing subsequently reused.

## The transferable part

**A wild array index carries its own provenance.** `-1007687205` is not noise:
it is one constant from the failing method's own table, and `length 6` picked
the 192-bit arm out of three. Converting the index to hex before theorising
named the value, the statement and the loop iteration, and every later
measurement only confirmed what that arithmetic had already said.

**When a guard is added to one accessor, ask which paths bypass that
accessor.** `pop_stack` grew a live-slot scan and the bug was closed there. The
splice does not go through `pop_stack`'s reclaim arm — it says so in its own
comment, at length, as the reason it computes the cursor itself — and that
comment was the map to the second instance of the same defect.
