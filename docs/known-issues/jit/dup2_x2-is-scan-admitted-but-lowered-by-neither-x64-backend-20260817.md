# `dup2_x2` is admitted by `jit_scan` and lowered by neither x64 backend, so a method containing one never compiles

## Status
**OPEN**, found 2026-08-17 on `dev` @`a276dfe09` by enumerating opcode-arm
coverage while investigating the bc-java PQC throughput page. Low impact as
measured (see below) — filed because it is a silent, permanent
never-compiles, and because the same species has already cost two
investigations.

## The gap

Comparing the opcode arms of the three walkers:

| walker | has an arm for `0x5e` (`dup2_x2`)? |
|---|---|
| `jit/src/x64/bytecode_compat.rs::jit_scan` (admission) | **yes** — advances `pc`, admits the method |
| `jit/src/ir.rs::IrBuilder::build` (optimizing tier) | no |
| `jit/src/x64/bytecode_walk.rs` (single-pass codegen) | no |
| `jit/src/aarch64_backend.rs` | **yes** — lowers it |

So on x86-64 a method containing `dup2_x2` passes admission, reaches the
single-pass dispatch loop, and falls into its final catch-all:

```rust
_ => {
    // Should not happen — jit_scan should have caught this
    return false;
}
```

That comment is wrong for this opcode. The method loses its compilation for the
life of the process, and the refusal is attributed to an arm that names nothing.

This is exactly the shape of the `pop2` (0x58) and `dup2_x1` (0x5d) gaps fixed
by the commons-math throughput work
(`fixed-suite-bugs/bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md`),
where a single missing arm kept two hot methods interpreted forever.

`frem` (0x72) and `drem` (0x73) are also scan-admitted and single-pass-unlowered,
but that is **deliberate and documented** at the scan site: the optimizing IR
backend lowers them via a call to the `jit_frem`/`jit_drem` helper, so admitting
them lets the IR pipeline see the method. `dup2_x2` has no such second home —
the IR builder does not lower it either.

`jsr`/`ret`/`wide`/`goto_w` are unlowered in *both* the scanner and the codegen,
so the scanner rejects those methods outright. That is the safe shape, and the
one `dup2_x2` does not have.

## Measured impact: small, but not zero

A `javap -c` sweep over BouncyCastle's `core`/`prov` main and test classes
(`org/bouncycastle/{pqc,crypto,util,math}`) found **three** `dup2_x2` sites:

| class | sites |
|---|---|
| `org/bouncycastle/crypto/engines/RC564Engine` | 2 |
| `org/bouncycastle/crypto/digests/WhirlpoolDigest` | 1 |

None is on the PQC path that motivated the search, and `WhirlpoolDigest` already
has a native override for its `processBlock`. So this did **not** contribute to
the PQC throughput cliff, and the page that found it
(fixed-suite-bugs/bc-java/bug-bcjava-pqc-lms-hsstests-interpreter-throughput-cliff-20260816-FIXED.md)
says so.

javac emits `dup2_x2` for a nested assignment whose value is category-2 sitting
above two more slots — rare in ordinary code, which is why the count is low.

## Fixing it

`aarch64_backend.rs`'s arm is **not** a safe template to copy. It pops four
operands unconditionally, which is only JVMS FORM-1 (all four category-1). In
the x64 operand model — one entry per *value*, not per JVM slot — the other
three forms have three, three and two entries respectively, so an unconditional
four-pop is wrong for them. (Whether that is also live on aarch64 is a separate
question this page does not answer.)

The `dup2_x1` fix is the right template: admit only the form whose width the
local oracle can *prove*, and leave the rest interpreted.

* `dup2_top_cat2` answers the top entry's width only.
* FORM 4 (both category-2) is two entries and is structurally `dup_x1` — but
  proving it needs the width of the **second** entry, which no current oracle
  answers.
* FORM 2 (top category-2, two category-1 below) is three entries and is
  structurally `dup_x2` — same missing witness.

So a correct fix needs a second-entry width oracle before any form can be
admitted. Given three known sites, none hot, that is the right amount of work
to defer — but the catch-all comment should stop claiming this cannot happen.

## The transferable part

**A "should not happen" arm is a claim, and claims about opcode coverage are
checkable.** Enumerating the arms of all four walkers took one script and found
a gap that four years of "jit_scan should have caught this" had asserted away.
Do it after every codegen change that adds or moves an opcode arm.
