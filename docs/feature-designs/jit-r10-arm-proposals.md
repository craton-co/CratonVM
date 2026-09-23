# JIT round 10, lane `arm` — proposals

Findings from an encoder-and-gate audit of the aarch64 backend
(`jit/src/aarch64.rs`, `jit/src/aarch64_backend.rs`) and of
`jit/src/backend_parity.rs` (x86-64 single-pass-vs-IR body comparison, owned by
this lane in this round though it has no aarch64 content). None of the three
directions below is a bug fix — the audit did not find a live encoding bug
worth fixing or filing (see the session report for what was checked and
refuted) — each is net-new mechanism or a small consistency cleanup.

## 1. A minimal AArch64 pseudo-op interpreter, for SEMANTIC differential tests

**What's missing today:** every aarch64 test in `jit/src/aarch64.rs` asserts
an exact instruction WORD. That is the right check for "did the encoder put
this field in the right bit position" (FOCUS item 1 of this round), and it
caught real bugs in earlier rounds (the register-31 aliasing, the load/store
immediate truncations). It cannot catch a bug where two fields are each
correctly encoded but the wrong VALUE was fed to the wrong field — e.g.
`emit_int_div_rem` handing `MSUB`'s `ra`/`rn`/`rm` operands in a permuted but
still-well-formed order, which would still assert as "a valid MSUB encoding"
under an exact-word test that only checks against a hand-computed expected
word using the *same* permutation. `aarch64_backend`'s own test suite already
works around this one instruction at a time by decoding fields back out of
the emitted word and asserting on the field VALUES against the source
`Arm64Instruction` (see `r9w11_div_rem_guard_the_divisor_and_branch_to_the_throw_stub`),
which is exactly the right idea but is re-invented per test.

**Mechanism:** a `#[cfg(test)]`-only, ~300-line interpreter in
`aarch64_backend.rs`'s test module (or a new `jit/tests/r10_arm_interp.rs` —
this lane's naming convention) that executes the `Arm64Instruction` pseudo-op
stream directly (not the encoded bytes — no ARM host or emulator needed),
against a `[i64; 31]` GPR file, an `[f64; 32]` FP file, and a `Vec<u8>` stack
scratchpad for `Ldr`/`Str` frame slots. It needs to cover only the ~45
pseudo-ops the backend actually emits (the match in `emit_machine_code_inner`
is the exhaustive list). Then EVERY existing `compile_with(...)` +
`eval_int_method`/`eval_...` test in `aarch64_backend.rs`'s suite — which
today runs the real encoded bytes on... nothing, because CI is x86-64, so
those tests currently only check `Arm64CompileResult::success` and inspect
the instruction list by pattern-matching — gains a SECOND, independent
oracle: interpret the pseudo-ops and compare against the interpreter tier's
actual result for the same bytecode (already computed in-process for every
compiled method). A mismatch means either backend's LOWERING is wrong, not
just its encoding — the class of bug this round's audit could not rule out by
inspection alone, because verifying `emit_lcmp`'s `CMP`/`CSET`/`CNEG`
sequence or `emit_int_div_rem`'s guard-then-divide shape by hand-tracing
values is exactly the kind of check a human reviewer is worst at and a
50-line interpreter loop is best at.

**Cost:** the interpreter itself is a big match on `Arm64Instruction`
variants (already an exhaustive, closed enum — no guessing needed) executing
each one's obvious arithmetic; a day, single-lane, no coordination. The
integration (running it from every existing arithmetic/branch/conversion test
instead of only checking `success`) is bigger — call it a full round — because
it means rewriting assertions test-by-test rather than adding one new test
file, and it is the only piece of this proposal that touches test files
outside this lane's normal review scope (needs its own round with this lane
holding `aarch64_backend.rs` alone, per the round's file-locking rule).

**Recommendation:** worth doing before the shared-memory lowerings
(`getfield`/`putfield`/`bastore`/the throw path) land for real, because that
work is exactly where a permuted-operand bug would hide — an NPE guard that
compares the wrong register, or a bounds check that adds instead of
subtracts, would still encode as "a valid CMP" and pass every exact-word test
in the file.

## 2. Extend `backend_parity`'s capability-loss detector to aarch64 bodies

**What's missing today:** `backend_parity.rs`'s whole mechanism — decode a
finished body, report a capability the OPTIMIZING tier lost relative to the
baseline — is architecture-general in design (`BodyProfile`, `regressions`,
the asymmetric-loss `ParityVerdict`) but hard-wired to x86-64 encoding in
`decode_one`/`decode_vector`/`one_byte_shape`/`two_byte_shape`. It exists
because a lowering that quietly stops emitting a capability the JIT relies on
is invisible until a benchmark regresses (the module header's own
motivating incident: `bastore` lowering directly, `sieve` silently losing its
6.4x SWAR path with no failing test). The aarch64 backend is *heading toward*
exactly that failure shape: the shared-memory gate's whole design is "one
pseudo-op arm now orders its own memory access (`getstatic`'s `LDAR`), more
will follow, and each is admitted to `opcode_has_ordered_lowering` as it
lands" — which is a sequence of lowerings, several backend contributors, and
several rounds, i.e. precisely the conditions under which "a body quietly
stopped emitting `LDAR`/`STLR`/`DMB`" is a plausible regression. Round 9
waves 12-13 made it concrete: `arraylength` and the fourteen primitive array
element accesses now lower, so real array/field bodies exist. (The
`ARM64_CAN_ORDER_MEMORY` this paragraph named was removed in wave 14 — a
single `bool` in front of the per-opcode list was both too weak and, for its
exclusive-access half, unreachable; see `ARM64_LOWERS_ACQUIRE_RELEASE`.) Today there is nothing that would notice; the only check is the
exact-word tests pinning each finished lowering, which say nothing about
whether the NEXT lowering that touches the same field keeps emitting the
barrier.

**Mechanism:** an `Arm64BodyCapability` enum (`Ldar`, `Stlr`, `Dmb`,
`Ldaxr`/`Stlxr`/`Casal` as one `Exclusive` bucket) and an aarch64
`profile_body`, MUCH simpler than the x86-64 one because AArch64 is
fixed-width: no length decode is needed at all, just
`code.chunks_exact(4)` and a match on each word's fixed top bits (LDAR/STLR
are already fully pinned bit patterns per `ldst_ordered`'s doc). This is a
few hours, not a redesign — the hard part of `backend_parity.rs` (variable-length
x86 decode, fail-closed on desync) doesn't exist on this architecture. The
comparison baseline is not another backend (aarch64 has no second tier to
compare against) but the PREVIOUS release's compiled body for the same
method, i.e. a golden-body regression test: compile a small fixed corpus of
volatile-field-touching methods once `getfield`/`putfield` land, record their
`Arm64BodyCapability` sets in a checked-in golden file, and fail if a future
change compiles the same method with fewer barriers. This is the aarch64
counterpart of the ratchet tests already in this tree
(`jit/tests/process_global_statics_ratchet.rs`) — same idea (a number that is
only allowed to move in the safe direction), applied to barrier presence
instead of static-declaration count.

**Cost:** small (the decoder) plus one golden-file corpus to build and
maintain as new opcodes get ordered lowerings; grows with the backend, not
with this change.

**Recommendation:** this was "stage it alongside step 1 of
`aarch64-backend-has-ordering-encoders-but-no-shared-memory-lowerings-20260918.md`
rather than after, while the lowering set is still small (one opcode:
`getstatic`)". That window has closed: round 9 waves 12-13 took the set to
sixteen opcodes (`getstatic`, `arraylength`, and the fourteen primitive array
element accesses), and that page is RETIRED
(`docs/internal/retired/...-RETIRED-20260921.md`). The proposal stands and is
now worth MORE, for exactly the reason it gave — it is scaffolding that is
expensive to retrofit once ten opcodes order memory and nobody remembers which
one lost its barrier. Start the golden corpus from the sixteen that exist.

## 3. Fold `ldr_imm_w`/`str_imm_w` into the A8 register-field type split

**What:** the A8 change (`4b83cd26a`) split every load/store base register
into `impl Into<RegSp>` and every zero-extending data register into
`impl Into<RegZr>`, and did so for `ldr_imm`/`str_imm` (64-bit),
`ldrb_imm`/`ldrh_imm`/`strb_imm`/`strh_imm` (the byte/halfword forms added in
round 9 wave 10) and the FP forms — but not for `ldr_imm_w`/`str_imm_w` (the
32-bit unsigned-offset forms), which still take a plain `Reg` for both `rt`
and `rn`. This is not a live bug: `Reg` cannot BE encoding 31 any more (A8
removed `SP` from the enum entirely), so there is no way to pass SP through
these two functions today, and their only production caller
(`aarch64_backend`'s `MemLoad`/`MemStore` `W32` arm) always passes an
allocator-assigned scratch register — never SP — as both `rt` and `rn`, so
the narrower type costs nothing today. But it is the exact shape of
inconsistency the A8 commit message itself flags as the dangerous half-finished
state ("mixed SP-reading immediate forms with XZR-reading shifted forms"):
if a future 32-bit lowering ever needs `[SP, #imm]` (a spilled `int` local
addressed directly off SP rather than FP, say), the fix has no home to go
to — the caller would have to either widen these two functions first
(discovering the gap by a compile error, the safe direction) or, worse, round-trip
through `RegSp::enc()`/a raw `u32` and re-derive the encoding by hand next to
the two functions that already do it correctly.

**Mechanism:** widen the two signatures to
`ldr_imm_w(&mut self, rt: impl Into<RegZr>, rn: impl Into<RegSp>, imm: u16)`
and the `str_imm_w` equivalent, exactly mirroring `ldr_imm`/`str_imm`'s
already-widened signatures three lines above them in the same file. Every
current call site passes a `Reg`, which already implements `Into<RegZr>` and
`Into<RegSp>`, so this is a pure widening with zero call-site changes.

**Cost:** minutes; it is a two-function signature change with no behavior
change on any existing caller. Not done in this round because HARD RULE 5
("keep edits compile-safe in isolation... do not change the signature... of
any public item other files use") makes it out of scope for a lane that
cannot build: `ldr_imm_w`/`str_imm_w` are `pub fn` on a type
(`Aarch64Emitter`) that `aarch64_backend.rs` also touches heavily in this
same round, and confirming no other in-flight lane's uncommitted edit takes a
narrower view of these two functions needs a build this lane is not allowed
to run.

**Recommendation:** low priority, but free — bundle it into whichever future
change first needs `[SP, #imm]` on a 32-bit access, rather than filing it as
its own round.
