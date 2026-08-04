# HIR-02 — retired 2026-08-04: the selector has somewhere to send its output

Branch `fix/hir02-mir-regalloc-handoff-20260804`, merged to `dev` and pushed.
Retires `docs/known-issues/c2/archive/hir-02-mir-regalloc-handoff.md`.

The brief's title was the whole ask: *give the instruction selector somewhere
to send its output*. It now has one. `CRATONVM_JIT=ir-isel-emit` (declared,
default off) makes `isel`'s tiles the emitted bytes, through a level-2 artifact
the emitter consumes, with a byte-equality oracle that ran over a real workload
and found **zero** disagreements.

Every increment the brief and its consolidated successor
(`docs/feature-designs/jit-machine-level-and-instruction-selection.md`) list is
either landed or closed with a measurement. One — increment 3, real registers —
is **deliberately not built**, and §6 says why in terms of that same document's
own refusal list.

---

## 1. What the brief asked for, and what happened to each item

| Brief item | Outcome |
|---|---|
| Increment 1 — the 32-bit immediate rows | Landed 2026-08-03 (eight rows), before this branch. |
| Increment 1b — `Rule::Lea` at `Ty::I32` | **Landed.** One anchored row. `Lea` fires **26 tiles** on the C2 corpus, against **zero** before. |
| "The first increment": a tile list, an allocation over it, an encoder | **Landed** as `MirPlan` + the frame plan + the allocation, encoded through `isel::select`. |
| "Prove equivalence rather than asserting it … compare emitted bytes" | **Landed** as `ir-isel-verify`, and it ran: 27 tiles, 19 methods, **0 mismatches**, checksums identical in all four modes. |
| Hazard 1 — safepoints | Discharged **structurally**: the emitted bytes are identical to the per-opcode arms', so no reference changes residency and `OopMapEntry` is untouched. |
| Hazard 2 — the vector register pool | **Landed** as `regalloc::xmm_roles` plus a caller-supplied `VecEmitRequest::vector_pool`. The overlap itself cannot be removed here; §5. |
| Increment 3 — registers | **Not built, on purpose.** §6. |

---

## 2. The level-2 artifact, and what it deliberately is not

`MirPlan` (`jit/src/ir_lower.rs`) is the selector's own `Vec<MInst>` per block
plus a `tile_of` index. The two side tables the design doc names — `SlotPlan`
for the frame, `RegResidency` for the allocation — stay exactly where they
were. There is no `mir/` directory, no crate, no trait hierarchy: the doc's
"What to refuse" list forbids all three, and nothing in the wiring wanted them.

The encoder is level 3 unchanged — `isel::select`, whose every row names the
hand-written emitter it reproduces byte for byte. It encodes against the
**frame-homed** allocation, which is not a simplification of a register
allocation but *this backend's* allocation, the one `ir_lower::frame_word_off`
states by returning `Err` for `ValueLoc::Reg`. Every value in its frame word,
RAX the destination, RCX the second operand.

Two modes, both declared (`types/src/flag_groups.rs`) and both default off:

| Flag | What it does |
|---|---|
| `CRATONVM_JIT=ir-isel-verify` | Builds the machine list; the **per-opcode arms still emit**; the encoder's answer is compared against what they wrote, per node. Changes no emitted byte, so a disagreement is a diagnosis rather than a wrong instruction. |
| `CRATONVM_JIT=ir-isel-emit` | The encoder emits. The per-opcode arm is not run for a node a tile covers. |

Both are **fail-closed**, which is the one thing increment 0 was explicitly
exempt from: a block selection that does not cover, a destination slot the
encoder and `alloc_slot` disagree on, or any byte mismatch discards the
artifact and the method runs in a lower tier.

Scope is `Rule::AluReg`, and only tiles covering exactly their own root. A tile
that **absorbed** a node must never reach the encoder — the caller skips every
node a tile covers, so an absorbed node the encoder does not fold would be
computed nowhere. `AluReg` never absorbs; the check is there because a rule
added to the set later might, and that is the same failure
`BlockSelection::covers` exists to make unrepresentable, one level down.

### Two refactors that removed a copy rather than adding one

* `load_to_rax` / `load_to_rcx` / `store_rax` now delegate to `enc_frame_load`
  / `enc_frame_store`. **A byte-equality oracle is only as strong as the number
  of places the bytes come from**; two frame accesses that agree today can
  drift, and the drift would be invisible to a test driving only one of them.
* `alloc_slot_checked` splits into `planned_slot_off` (pure) plus the three
  mutations, so the encoder can address a destination *before* it is allocated
  and get the allocating path's answer by calling it rather than by repeating
  the arithmetic.

---

## 3. The measurement, which is the deliverable

Azure Linux, release binary, `bench/CratonBenchC2.java` — the corpus MEAS-02
built precisely because CratonBench's kernels do not reach the optimizing tier.
Framework-shaped node mix, three phases, one process each.

**Coverage, after the two row gaps closed:**

| | dispatch | bind | pipeline |
|---|---:|---:|---:|
| methods shadowed | 9 | 5 | 5 |
| scheduled data nodes | 140 | 75 | 96 |
| **nodes covered by a real rule** | **33 (23.6%)** | **18 (24.0%)** | **31 (32.3%)** |
| `Rule::Lea` tiles | 12 | 4 | 10 |
| `covers()` violations | **0** | **0** | **0** |

`Rule::Lea` fired **zero** times across 850 Spring Boot methods before the
`lea_r32_m` row existed. It is now the most frequent non-generic rule on this
corpus. Two cautions on the coverage figures: this is a different corpus from
the 15.7–19.0% Spring Boot measurement, so the two are not a before/after
pair — and the shadow pass measures with `SelectOptions::default()`, i.e.
*without* the frame-homed pricing the emitter uses.

**The oracle, and correctness:**

| | dispatch | bind | pipeline |
|---|---:|---:|---:|
| methods through the machine list | 9 | 5 | 5 |
| tiles the encoder emitted | 12 | 6 | 9 |
| **byte mismatches** | **0** | **0** | **0** |
| tiles it could encode but may not emit | 5 | 0 | 7 |
| …bytes the per-opcode arms wrote for them | 115 | — | 150 |
| …bytes the encoder would have written | **85** | — | **114** |

Every phase produced a **bit-identical checksum** in all four modes (off,
shadow, verify, emit): `2893201123071733440`, `-1727289071355132288`,
`97968176938830464`.

The last three rows are increment 2b's price tag, and they are the reason to
keep the machine level rather than delete it: **265 bytes → 199 over 12 nodes,
a quarter smaller**, on rules increment 2's oracle cannot cover. That is a
number, not an argument, and it is what the next lane should be sized from.

They also explain why the emitted-tile count *fell* from 39 to 27 when the
operand pricing landed: those twelve nodes were only `Rule::AluReg` because the
cost model could not see the load. Fewer tiles emitted, and a truer tiling.

---

## 4. Three findings that were not in the brief

**(a) Closing one gap re-ranked the tiling, and the cost model was measuring
the wrong thing.** The moment `lea_r32_m` landed, `a + b` started selecting an
`LEA` — because `needs_copy` priced a two-address `MOV`. Under a frame-homed
allocation that `MOV` does not exist: the destination register never held the
left operand, so "copy it there" and "load it there" are the same instruction.
The `LEA` that won is a byte *longer* than the `ADD` it replaced.
`SelectOptions::frame_homed` now states the allocation, and `Tile::frame_homed`
re-prices **every** candidate from its own operand set — applied in `admit`, so
a rule that forgot cannot look cheap. (The first cut charged only `AluReg` and
handed every add straight back to `Lea`: charging one rule and not its
competitors is a thumb, not a cost model.)

**(b) `Rule::AluImm` was never blocked by the rows.** Increment 1 added them
and then recorded `AluImm` firing once in 719 methods, attributing it to
`ir_optimize` folding constants away. Two further causes, both measured here:

* `ValueUses::single_use` is `count == 1` **and not pinned**, and a safepoint
  snapshot names almost every live constant. So the tile usually cannot
  *absorb* the constant even when it can use it as an immediate.
* With the constant unabsorbed the two forms compete over one node, and
  `ADD EAX, ECX` is two bytes against `ADD EAX, 7`'s three. The register form
  wins on the only axis the cost model measured — while emitting four bytes
  *more*, because it also loads the constant into a register. Pricing operands
  under `frame_homed` is what makes the immediate form win.

**(c) A source-scanning gate failed open, and the edit that did it was
innocent.** `the_register_read_path_is_gated_on_publication` scanned
"everything up to the first `#[cfg(test)]`" — a *position*, not a boundary. A
test-only helper added anywhere above `set_residency` truncates the scanned
region and the count collapses to zero. The boundary is now the test module,
named, with a precondition that fails if the corpus goes missing. This is the
same lesson as `reference_source_scanning_gates_name_files_not_modules`,
arriving from a different direction.

---

## 5. Hazard 2 — the vector register pool

`vec_emit::emit_vector_loop` carried a private XMM0–5 pool that is *also*
`ir_lower`'s FP scratch pair (XMM0/XMM1) and its entire linear-scan file
(XMM2–XMM5). Wire it to anything and a scalar `double` in XMM3 is destroyed by
a vector region that took XMM3 for a lane accumulator — silently, with a wrong
number arriving much later.

What landed:

* `regalloc::xmm_roles` declares all three ranges **together**. `ir_lower`'s
  `XMM0`/`XMM1`/`IR_LOWER_LS_XMMS` are now defined *from* it, so there is one
  declaration rather than three that happen to agree.
* `VecEmitRequest::vector_pool` — the registers the caller guarantees are dead
  across the region — replaces the private constant. An **empty pool is legal**
  and refuses at the first allocation, which is the right answer for a caller
  that has not done the analysis.
* A pool naming a register the emitter cannot encode without a prologue save
  area refuses the **whole region** rather than being quietly narrowed: a
  narrowed pool leaves the two parties disagreeing about which registers are
  live.

What did **not** land, and cannot here: the overlap itself. The only registers
that would separate the ranges are XMM6–XMM15, callee-saved on Windows, and
`ir_lower::emit_prologue` saves no register at all — which is increment 3's
prerequisite. `the_three_xmm_authorities_are_stated_in_one_place` therefore
*asserts* the overlap, and will fail the day a prologue save area removes it.

---

## 6. Increment 3 — registers — is not built, and this is the reason

The brief and the design doc both name the prerequisite correctly:
`ir_lower::emit_prologue` saves no callee-saved register, so RBX/R12–R15 cannot
be handed out until there is a save area restored on all three exits.

That is not the reason it is unbuilt. Building the save area is tractable. The
reason is that a save area with no consumer would be a **fourth** finished
component with no caller — in a lane whose entire finding is that this compiler
already has three — and the change that would give it a consumer is on the
design doc's own **"What to refuse"** list:

> A big-bang conversion of `ir_lower`'s 90 GP memory accesses. That is the
> "stop treating the frame as the value's identity" change — the whole item,
> not an increment.

Handing out a GP register means every read site in `ir_lower` has to consult
residency, `frame_word_off` has to stop returning `Err` for `ValueLoc::Reg`,
and the safepoint rule — *no reference register-resident at a GC safepoint* —
stops being structural (today `ir_lower` satisfies it by having no GP register
to give) and becomes something that has to be proved per site. The prologue is
the small half.

What would change this: a measurement showing the frame round trip is a
material cost on real code. This lane did not produce one, and the honest
statement is that it did not look — the byte-equality oracle it built cannot
answer a performance question by construction.

---

## 7. Verification

* `cargo test -p cratonvm-jit --lib` — **1906 passed, 0 failed** (Azure Linux).
* CratonBenchC2 × 3 phases × 4 modes (off / shadow / verify / emit): identical
  checksums, 0 mismatches, 0 `covers()` violations (§3).
* New tests that can actually fail, each with its trip condition written down:
  * `the_machine_level_emits_the_same_bytes_as_the_per_opcode_arms` — the
    oracle at unit scale, plus a non-vacuity assertion on the tile count.
  * `an_injected_wrong_byte_is_caught_by_the_oracle` — corrupts a tile and
    asserts both that verify mode counts it **and** that the compile is
    refused. A guard nobody has seen fail is a guard of unknown polarity.
  * `the_machine_level_is_off_by_default` — the counter, not the bytes: "did
    not execute" and "changed no byte" are different claims.
  * `frame_homing_removes_the_copy_that_makes_lea_win`,
    `frame_homing_makes_the_immediate_form_win` — the option earning its keep.
  * `the_32bit_lea_row_reproduces_the_imul_const_byte_literals` — the anchor.
  * `a_pool_naming_a_windows_callee_saved_register_refuses_the_region`,
    `an_empty_pool_refuses_instead_of_helping_itself_to_the_scalar_file`,
    `the_three_xmm_authorities_are_stated_in_one_place`.

`regression-suite/run.sh` was run on the same host and is **not usable as a
verdict**: another session's Spring Boot suite held the 16-core box at load
51–242 throughout, and three of the four CratonVM failures are `rc=124`
timeouts, with a fourth (`RMapResizeGc`) reported as "output differs from
HotSpot" while the HotSpot section is **empty** — the oracle timed out, and
CratonVM's own line says `PASS`. The two non-timeout failures were baselined
the cheap way and are JIT-independent: `RSerial` and `RFileTimes` fail
**identically under `--nojit`**, which excludes every change on this branch.

The load-insensitive evidence is what this section rests on instead: 1 906 unit
tests, and three checksummed workloads compared across four modes.

**Pre-existing red on `origin/dev`, baselined on an unmodified checkout of it
and not caused by this branch:**

* `cratonvm-types --test flag_declaration_guard` —
  `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS` and `CRATONVM_JIT_OSR_SINGLE_PC` are read
  at `jit/src/lib.rs:3864` / `:3876` and declared nowhere. Rule 4's recurrence,
  again.
* `cratonvm-types --test doc_citation_paths` — one citation pointing at a page
  another session moved.
* `tools/flag-census/render-inventory.py` refuses to regenerate
  `docs/config/flag-inventory.md` because `CRATONVM_JIT_C1_VECTOR_VETO` is in
  `INVENTORY` and not in `types/tests/flag-surface.txt`. The two new flags in
  this branch are in both, and in `docs/flag-tokens.md`.

---

## 8. Where the residual now stands

`docs/known-issues/c2/README.md`'s residual row for `hir-02` was:

> six 32-bit isel pattern rows; `Rule::Lea`/`AluImm` fire zero times on real
> code

Both halves are closed, and the second one had a different cause than the row
list implied (§4b). What is open after this lane, in the consolidated doc
rather than here:

* **Emitting the improving rules.** `Rule::AluImm` and `Rule::Lea` cannot ride
  the byte-equality oracle by construction — dropping a frame load is the
  point, so the bytes differ. They need a differential-execution oracle, which
  is `verify-01`'s harness, and a decision about whether the saving is worth
  it — and verify mode has already priced it: **265 bytes → 199 over 12 nodes**
  on this corpus (`shadow_tiles`, `arm_bytes`, `enc_bytes` on the
  `[ir-isel] MIR TOTALS` line).
* **Increment 3**, on the terms in §6.
* **The XMM overlap**, which increment 3's prologue would remove (§5).
