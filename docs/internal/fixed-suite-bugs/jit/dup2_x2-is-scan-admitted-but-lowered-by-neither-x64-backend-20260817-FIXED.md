# `dup2_x2` was admitted by `jit_scan` and lowered by neither x64 backend, so a method containing one never compiled

**Status: FIXED 2026-08-18** on `fix/jit-dup2x2-x64-lowering-20260818`, branched
from `dev` @`77d8faf96`. Found 2026-08-17 on `dev` @`a276dfe09` by enumerating
opcode-arm coverage while investigating the bc-java PQC throughput page.

The OPEN page's diagnosis was exactly right, and so was its analysis of what a
correct fix needs. Its *conclusion* — "that is the right amount of work to
defer" — was wrong, for a reason it could not have known: the second-entry
width oracle it said did not exist had been in the tree since
`osr-entry-unresumable-exit-FIXED-20260803`, under a different name and built
for a different consumer.

## The gap, as found

| walker | arm for `0x5e` (`dup2_x2`)? |
|---|---|
| `jit/src/x64/bytecode_compat.rs::jit_scan` (admission) | **yes** — advances `pc`, admits the method |
| `jit/src/x64/bytecode_walk.rs` (single-pass codegen) | no |
| `jit/src/ir.rs::IrBuilder::build` (optimizing tier) | no |
| `jit/src/aarch64_backend.rs` | **yes** — and lowered it wrong |

On x86-64 a method containing `dup2_x2` passed admission, reached the
single-pass dispatch loop, and fell into its final catch-all:

```rust
_ => {
    // Should not happen — jit_scan should have caught this
    return false;
}
```

The method lost its compilation for the life of the process, and the refusal
was attributed to an arm that names nothing (`take_jit_bail_site` reported the
bare `singlepass-codegen`).

## What the fix is

### 1. The second-entry width oracle already existed

`dup2_top_cat2` reads the one instruction before the dup, so it can only ever
answer for the TOP entry. `dup2_x2`'s four JVMS forms differ in how many
COMPACT entries their operands occupy — one entry per *value*, so a category-2
`long`/`double` is one entry and two JVM slots:

|  | shape | entries duplicated | insertion depth |
|---|---|---|---|
| FORM 4 (`v1`,`v2` cat-2) | `[v2, v1] → [v1, v2, v1]` | 1 | 2 |
| FORM 2 (`v1` cat-2) | `[v3, v2, v1] → [v1, v3, v2, v1]` | 1 | 3 |
| FORM 3 (`v3` cat-2) | `[v3, v2, v1] → [v2, v1, v3, v2, v1]` | 2 | 3 |
| FORM 1 (all cat-1) | `[v4, v3, v2, v1] → [v2, v1, v4, v3, v2, v1]` | 2 | 4 |

The top's category picks the left-hand column; the entry *below the duplicated
group* picks the right-hand one. The OPEN page concluded that "no current
oracle answers" the second question.

`x64::stack_kinds` answers it. It is a forward abstract interpretation over the
bytecode that tracks the KIND of every entry of the same compact operand stack
the backend simulates, index-aligned with `Compiler::stack`, already run
immediately before the walk (`driver.rs`) and already consulted by the deopt
snapshot encoder. It declined the whole category-dependent dup family —
"not modelling them costs precision, guessing them costs correctness" — which
is why an arm-by-arm read of the *codegen* did not find it.

Modelling `dup_x2` / `dup2` / `dup2_x1` / `dup2_x2` there costs neither: every
form is decided by categories the analysis already tracks, and `Unknown` still
poisons rather than guesses. It also stops poisoning every bci downstream of a
dup, which is a free precision win for the OSR/deopt encoder that owns the
analysis.

### 2. Reading a snapshot oracle to pick a codegen SHAPE

That is a stronger use than typing a snapshot, so `stack_entry_categories`
admits an answer only under three independent cross-checks:

* **DEPTH** — the analysis derives it from the JVMS stack effects, the emitter
  from running its own opcode handlers.
* **REF-NESS** — every entry the analysis calls a reference must be one the
  emitter's own GC oop mark also calls a reference. Different provenance;
  catches an off-by-one that preserves depth.
* **THE TOP ENTRY** — when `dup2_top_cat2` answers, it must agree. A wholly
  independent peephole opinion. A disagreement means one of the two analyses is
  wrong, so neither is used.

Any disagreement, or an `Unknown` in a deciding position, stays interpreted
under `singlepass-codegen/dup2_x2-unprovable-form`. The arm additionally checks
the deepest entry of the form it picked (`v3` for FORM 2, `v4` for FORM 1),
which verified bytecode guarantees and which costs one lookup.

`CRATONVM_JIT_NO_DUP2_X2` bisects the new arm alone.

### 3. aarch64 was worse than the page suspected

The page flagged that `aarch64_backend.rs` pops four operands unconditionally —
FORM 1 only — and said "whether that is also live on aarch64 is a separate
question this page does not answer". It was live, in five arms, and there was a
second defect the page had no way to see:

**this backend keeps two simulated operand stacks.** `operand_stack` holds
int/long/reference; `float_operand_stack` holds float/double. All nine stack
shuffles (`pop`, `pop2`, `dup`, `dup_x1`, `dup_x2`, `dup2`, `dup2_x1`,
`dup2_x2`, `swap`) popped a fixed number of entries from the first one:

* A float/double operand is on the *other* stack. `iconst_0; iconst_1;
  fconst_0; dup` duplicated `iconst_1` and left the float untouched — no
  underflow, no refusal, a silently wrong operand stack whenever the integer
  stack happened to be deep enough. (The all-FP cases refused only by accident,
  through an integer-stack underflow; that accident is why a probe written
  against them alone passes pre-fix and proves nothing.)
* A `long` is one entry and two JVM slots, so `pop2` / `dup2` / `dup_x2` /
  `dup2_x1` / `dup2_x2` touched the wrong entry count on every form but the
  all-category-1 one. `[int, long] pop2` discarded the `int`.

All nine now go through `int_stack_shuffle_entries`, which reads the same
`stack_kinds` analysis and refuses the method when an operand is
floating-point or its category is unproven. Empty metadata suffices there:
this backend refuses every method containing a field access, a call of any
kind, or any `ldc`, which are the only sites the analysis needs metadata for.

### 4. The IR tier was deliberately left alone

`IrBuilder::build` still has no arm for `pop2` (0x58) or any of
`dup_x2`/`dup2`/`dup2_x1`/`dup2_x2`/`swap` (0x5b–0x5f). That is a **safe bail
that costs nothing here**: a `None` from the IR builder falls through to the
single-pass backend (`lib.rs`, "went single-pass"), which now lowers all six.
Widening the IR tier onto this population would be a coverage change with the
`cov-02` blast radius — an IR body installed over a single-pass body that
vectorises — and none of it is needed to close this page.

## Verified on the real sites

The `javap -c` sweep is confirmed: three `dup2_x2` sites in BouncyCastle, in
`RC564Engine` (2) and `WhirlpoolDigest` (1), and **all three are FORM 2** —
`dup2_x2; lastore`, javac's chained array assignment
`longArr[i] = otherArr[j] = v`, which is the only form javac emits at all. The
other three forms are reachable only from hand-written or non-javac bytecode
and are covered by unit tests.

Both probes were run on ONE binary
(`target/release/cratonvm-dup2x2-20260818.exe`, md5
`39af4448818e3e7cbbedf83856bed173`) with `CRATONVM_JIT_NO_DUP2_X2` as the only
variable, so this is an A/B and not a cross-binary comparison.

**`RC564Engine.setKey([B)V`**, driven 20,000 encrypt/decrypt round trips:

| arm | what the JIT log says |
|---|---|
| kill switch ON (the pre-fix behaviour) | `compile-bail RC564Engine.setKey([B)V backend_attempted=true reason=singlepass-codegen/dup2_x2-unprovable-form(pc=181,op=0x5e)` — and then `bg-direct-call DECLINED …: eager-callee-chain-compile-declined` |
| kill switch OFF (fixed) | `full-compile RC564Engine.setKey([B)V entry=… len=10975`, `bg-direct-call BOUND` |

`pc=181` is exactly the first of the two sites `javap` reports. The knock-on in
the first row is worth naming because the OPEN page did not: the refusal did
not only cost `setKey` its own body, it cost the CALLER its direct-call
binding.

Ciphertext checksum `343177855` on both arms and on HotSpot 25 — the fix
changes what compiles, not what it computes.

**A synthetic FORM-2 probe** (`Dup2X2Probe.chainedArrayStore`, a `long[]`
chained store in a loop), three interleaved runs, 3,000,000 iterations:

| run | arm ON | arm OFF |
|---|---|---|
| 1 | 2,711 ms | 25,037 ms |
| 2 | 660 ms | 14,895 ms |
| 3 | 603 ms | 13,381 ms |

Read that as what it is: a microbenchmark whose hot method is almost nothing
but the shuffle, and whose `main` happens to carry a `dup2_x2` of its own, so
the OFF arm also loses `main`'s OSR body (`OSR-bail … pc=117 opcode=0x5e`).
It is not a claim about any real workload. It is a clean demonstration of the
shape: **one `dup2_x2` anywhere in a method costs that method its compilation
for the life of the process**, OSR included, which is why a one-shot `main`
with a hot loop is the worst case.

On the actual measured population the OPEN page's verdict stands unchanged and
is repeated here so nobody re-measures it: **small, but not zero.** Neither
bc-java site is on the PQC path that motivated the search, and
`WhirlpoolDigest.processBlock` already has a native override, so this did not
contribute to the PQC throughput cliff and the page that found it says so.

The reason to fix it was never the three sites. It was that this is a **silent,
permanent never-compiles** whose refusal names nothing, and that the same
species had already cost two investigations (`pop2` and `dup2_x1`, the
commons-math throughput cliff).

## The transferable part, now enforced

**A "should not happen" arm is a claim, and claims about opcode coverage are
checkable.** The OPEN page proved that by hand, with one script, and found a
gap that four years of "jit_scan should have caught this" had asserted away.

`x64::tests::scan_admitted_opcodes_are_lowered_or_declared` is that script, as
a test. It probes admission BEHAVIOURALLY — a one-instruction body per opcode
handed to the real `jit_scan`, so the scanner's table is never transcribed and
cannot drift — and parses the walk's own top-level dispatch arms out of the
source at compile time, counting only arms at the match's own brace depth so a
nested `match op` cannot forge coverage. Run against pre-fix `dev` it names
`0x5e` and nothing else: the page's hand-enumerated finding, reproduced.

`frem` (0x72) and `drem` (0x73) are the only declared exemptions, each with the
reason it is not this shape — the optimizing IR backend lowers them via a call
to the `jit_frem`/`jit_drem` helper, so admitting them buys a compilation. The
bar for that list is a **second home**; "nobody lowers it anywhere" is the
`dup2_x2` shape and belongs in an arm. The list is a ratchet in both
directions: an entry that later grows an arm fails the test, so a stale
exemption cannot hide the next real gap.

The catch-all itself no longer claims to be unreachable. It records
`singlepass-codegen/opcode-scan-admitted-but-unlowered`, so if one ever does
land there it is at least legible in the bail record instead of arriving as a
bare `singlepass-codegen`.

## Tests

Every one of these fails against pre-fix `dev`; the four x64 form tests and the
three aarch64 count tests were run against a spliced pre-fix backend to confirm
it.

| test | what it pins |
|---|---|
| `x64::tests::dup2_x2_form{1,2,3,4}_*` | each form COMPILES and RUNS, checked against a value a wrong-width shuffle cannot produce |
| `x64::tests::dup2_x2_without_a_provable_form_bails_under_its_own_name` | the conservative half, named |
| `x64::tests::the_unlowered_opcode_catch_all_names_itself` | the catch-all's reason string |
| `x64::tests::scan_admitted_opcodes_are_lowered_or_declared` | the coverage guard |
| `x64::tests::the_dispatch_arm_parser_reads_the_real_match` | the guard is not vacuous — the parser finds >150 arms and does not invent coverage |
| `x64::stack_kinds::tests::dup2_x2_form{1,2,3,4}_*`, `dup2_*`, `dup_x2_*` | the exact post-shuffle kind vector for every form of every dup |
| `x64::stack_kinds::tests::dup2_x2_over_an_unknown_third_entry_poisons` | rule 2 of the analysis's safety argument still holds for the new arms |
| `aarch64_backend::tests::dup2_x2_form{1,3,4}_*`, `pop2_over_a_long_*`, `dup2_over_a_long_*` | entry counts, and that the entries beneath a shuffle are not consumed |
| `aarch64_backend::tests::a_floating_point_operand_refuses_the_shuffle_*` | the two-stack defect, with the non-underflowing cases first so the test cannot pass for the wrong reason |
| `aarch64_backend::tests::an_untypeable_shuffle_refuses_the_method` | the conservative half on aarch64 |

## What is still open

Nothing on this page. Two adjacent facts, recorded so nobody re-derives them:

* The IR tier's missing dup-family arms (§4) — a safe bail, not a defect, and
  deliberately not widened.
* `licm::dup2_category_safe` is dead code (`#[allow(dead_code)]`, "no longer
  wired into `jit_scan`"). Its comments asserted that the codegen has no arm
  for `pop2`/`dup_x1`/`dup_x2`/`dup2_x1`/`dup2_x2`; all five are wrong now and
  have been corrected in place rather than deleted, because the helper's own
  history is the argument for its policy.
