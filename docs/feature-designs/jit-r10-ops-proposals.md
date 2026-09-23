# JIT review round 10, lane `ops`: proposals

**Status:** proposals only, nothing landed. Written after a correctness pass
over the single-pass backend's per-opcode arms (operand-stack shape,
array/field width, arithmetic edges, control flow, and the scan-vs-lowered
structural parity) found the arms already correct — see the round's report
for what was checked. These are mechanisms noticed along the way, not fixes
for defects.

## 1. Elide the redundant leading `MFENCE` on a volatile `putstatic`

**File:** `jit/src/x64/op_field.rs`, the `0xb3` arm, lines ~262-264 and
~316-318.

**What is true today:** a volatile `putstatic` emits `MFENCE` **before** the
call to `jit_putstatic_*` (line 264, labelled "SeqCst release") and again
**after** it (line 318, labelled "SeqCst store-load barrier"). On x86-64 TSO,
every store already has release semantics for free — the JMM only needs a
fence to stop the volatile store from being reordered with a *later* volatile
load (StoreLoad), which the trailing fence alone already provides. The leading
fence adds a second full serializing instruction (`MFENCE` is ~20-40 cycles on
recent Intel/AMD) to every volatile static write for no JMM requirement it is
the one covering.

**Cost of leaving it:** every volatile `putstatic` site pays two `MFENCE`s
instead of one. `jit/src/x64/objects.rs`'s volatile `putfield` path (per the
comment at `op_field.rs` line ~20-29) already gets this right — one MFENCE,
after the store — so `putstatic` is the outlier, not the model.

**Why unfixed here:** this lane owns `op_field.rs`'s codegen arm but not the
measurement to confirm removing the leading fence is neutral under the actual
JMM stress suite (`RVolatilePublish`-style tests), and a JMM-visible ordering
change should not go in on inspection alone, per this round's rule against
claiming a verification that was never run.

**What it would take:** drop the `MFENCE` at line 264 (keep the one at line
318), then run the volatile-field JMM stress suite (interpreter vs compiled,
concurrent publish/consume) A/B against a build with and without the leading
fence, on a host with no other load (see the "sequential A/B invents a
regression" family of findings — this needs the concurrent-arm discipline,
not a quick sequential run).

## 2. Give the CMP-chain / jump-table switch crossover a shared, named constant on the field/array cache boundary too

**File:** `jit/src/x64/op_control.rs`, `0xaa`/`0xab` arms — already uses
`crate::ir::SWITCH_CHAIN_MAX_CASES` shared with the optimizing tier's
`Op::Switch` lowering (good: this round found nothing wrong with it). The
`lookupswitch` arm's binary-search/chain crossover, though, is a **local**
literal `6` (`if npairs <= 6 || !sorted`, `op_control.rs` line ~1081) with no
shared constant and no comment cross-referencing whether the optimizing tier
picks the same number for its own `Op::Switch` lowering of a sparse table.

**Why this matters:** `tableswitch`'s crossover is explicitly kept in sync
with the IR tier "because the two tiers agreeing about where an indirect
branch starts paying for itself is worth more than either being individually
tuned" (the arm's own comment). `lookupswitch`'s crossover has no such
statement, so nothing prevents the two tiers from silently picking different
numbers for the sparse-switch shape and disagreeing about which lookupswitch
sizes are worth a binary search versus a chain — the same class of tier
disagreement `single_pass_only.rs` was created to make visible for
*capability* gaps, but this is a *tuning* gap, which that file does not cover.

**What it would take:** promote `6` to a second `crate::ir::` constant
(e.g. `LOOKUPSWITCH_CHAIN_MAX_PAIRS`) read by both this arm and the IR tier's
lookupswitch/sparse-switch lowering, plus a short benchmark run (the crossover
was presumably chosen empirically once; re-validating it costs one A/B run
against a synthetic sparse-switch microbenchmark at a few `npairs` values
around 6).

## 3. A standing regression test for the scan-admitted / lowered opcode tables

**Files:** `jit/src/x64/bytecode_compat.rs` (`jit_scan`'s admitted opcode set)
and `jit/src/x64/bytecode_walk.rs` (the `WalkFamily` dispatch table feeding
`walk_control`/`walk_field`/`walk_array`/`walk_object`/`walk_arith`/
`walk_local_stack`).

**What is already true:** `bytecode_walk.rs`'s dispatch loop already has a
named catch-all (`_ => { self.fail("...opcode-scan-admitted-but-unlowered");
... }`, around line 1353) whose own comment says the two tables are compared
by a test, `x64::tests::scan_admitted_opcodes_are_lowered_or_declared` — and
this round's manual cross-check of every opcode in both tables (0x00 through
0xc8, all documented families) found them in full agreement, so this is not a
gap today.

**Why it is still worth a proposal:** that test lives in `jit/src/x64/tests.rs`,
which this lane cannot touch (owned by another lane / off-limits to every
lane per the round's rules), and *only* checks the codegen-vs-scan pairing.
It does not check the THIRD table this round found: `x64/bytecode_walk.rs`'s
`WalkFamily` map itself against the family functions' own internal `match`
arms (e.g. an opcode routed to `WalkFamily::Array` that `walk_array`'s match
does not have a case for falls through to `walk_array`'s own
`"walk-family-misdispatch"` catch-all at runtime, not at compile time or in a
static test). A three-way static check — scan table, family-routing table,
and each family function's arm set — would catch a family-routing edit that
sends an opcode to the wrong per-family function before it reaches a runtime
`fail()`. This is a proposal for the test-owning lane, not something to build
here.
