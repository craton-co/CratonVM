# OSR entry metadata: the contract between the pieces

**Status:** Shipped (always on — the check is ungated).

## What it does today

OSR entry itself has worked for a long time. What this contract adds is a
**publication-time check** over the vectors it rides on, because those vectors
span more than one coordinate space and a plausible integer in the wrong space
reads as valid.

- `jit/src/osr_coords.rs` gives the spaces distinct types — `OutPcIndexed<T>`
  and `BciIndexed<T>` — with `from_translated` and a checked
  `into_bci_by_identity`, plus a `note_mismatch` / `osr_coordinate_mismatches()`
  counter. The third space (JVM local index) is deliberately not modelled here.
- `jit/src/osr_contract.rs` provides `check_at_publication` and
  `osr_contract_violations()`. It enforces two invariants: the bci-indexed
  vectors agree in length, and every local the entry trampoline reads is
  assigned.
- The check runs at the real publication site, `jit/src/x64/osr.rs`. **A
  violation publishes no OSR metadata for that method** — the method still
  compiles, it just never enters via OSR. Failing closed matters because a
  short dead mask otherwise reads as "safe" through `unwrap_or(0)`.
- Counters are surfaced through `jit/src/tiered.rs`.

The master OSR switch is `CRATONVM_JIT_OSR` (`osr_backedge_enabled()` in
`vm/src/runtime/env_cache.rs`), default on.

## What is not built yet

- **There is still more than one door to "produce an OSR-capable artifact."**
  The OSR path reaches the backend directly rather than through a single
  admission funnel, which is how two of the doors came to carry hand-copied
  subsets of the admission chain in the first place.

## Goal

Make the relationships between the OSR metadata vectors checkable, and make a
disagreement between them refuse OSR rather than produce a plausible entry.

---

## Current state

### The machinery

`osr_pc_to_native` (per-bci entry table, `-1` = refused), `osr_dead_mask` (same
indexing), `osr_local_assignments` / `osr_xmm_assignments` (where the trampoline
seeds each local), `can_osr_enter` / `can_osr_enter_with`, `osr_trampoline`,
`osr_entry_frame_state`, `osr_exit_points`. Publication is one site in
`jit/src/x64.rs`; entry is `CompiledMethod::osr_enter` in `jit/src/lib.rs`.

### There are THREE coordinate spaces, not two

The brief says two and asks for an assertion that `osr_pc_to_native`,
`osr_dead_mask` and `osr_local_assignments` "agree on length". **Two of those
three are not in the same space**, so that assertion cannot be written as
stated — a length check across two spaces would pass or fail for reasons
unrelated to anything.

| Space | Indexed by | Vectors |
|---|---|---|
| **Interpreter bci** | a pc in the ORIGINAL bytecode | `osr_pc_to_native`, `osr_dead_mask` |
| **Output pc** | a pc in the emitter's (possibly rewritten) bytecode | `Compiler::osr_entry_native` before publication; `LoopXform::osr_entry_pc`'s result |
| **Local index** | a JVM local slot | `osr_local_assignments`, `osr_xmm_assignments` |

The runtime indexes the first two by interpreter bci. The emitter writes them at
whatever pc it is emitting, which under a bytecode transform is an *output* pc.
The conversion is `LoopXform::rebuild_pc_to_native` plus the pointwise
`osr_entry_pc` remap of the dead mask, both at the publication site.

### The four near-misses, re-verified

| # | The brief's claim | State |
|---|---|---|
| 1 | `osr_pc_to_native` is bci-indexed by the runtime but output-pc-indexed by the emitter | **Handled and now TYPED apart.** `jit/src/osr_coords.rs` — `OutPcIndexed` / `BciIndexed`, with the identity conversion checked against `orig_code_len`. |
| 2 | The `-1` refusal sentinel was not enforced as an invariant | **Fixed before this lane.** The publication site re-imposes `-1` at every bci `osr_entry_pc` refuses, "regardless of which producer filled the vector". |
| 3 | The OSR compile path calls the backend directly, not through `try_compile` | **Fixed.** `jit/src/compile_gate.rs` is the one door; all three call it, the backend *requires* the token (so skipping it is a compile error), and an entry made under the test escape hatch is still counted. It still calls the backend directly — what it can no longer do is skip the admission chain. |
| 4 | `osr_entry_frame_state` and the deopt frame state are unchecked against each other | **Fixed.** `CompiledMethod::osr_home_disagreement`, consulted by `validate_osr_entry` before the per-slot loop. |

---

## Design

### What is checked, and why each one

`jit/src/osr_contract.rs` — deliberately its own module, because the doc asks
for the properties to be asserted "against the *mapping function* over a
synthetic vector, not against whatever the emitter happened to place there", and
a check that can only be handed a real `CompiledMethod` cannot be tested that
way.

**1. The two bci-indexed vectors have equal length.** The one that is silently
*unsound* rather than merely wrong. `can_osr_enter_with` reads the dead mask
with `.get(entry_pc).copied().unwrap_or(0)` — so a dead mask shorter than the
entry table reads as "no dead locals" for every bci in the tail, and those
entries are admitted as safe. The trampoline then seeds a dead local over a live
one sharing its register. Nothing downstream can notice.

The edit that produces it is easy to make by accident: size `osr_dead_mask` from
the OUTPUT code length while `osr_pc_to_native` is rebuilt to the ORIGINAL one.
That is exactly the coordinate confusion of item 1, in the one place where it
does not announce itself.

**2. Every local the trampoline will seed has an assignment slot.**
`osr_trampoline` loops `for i in 0..num_locals` reading `assignments.get(i)`, so
a short vector degrades to "no register home" silently. Not memory-unsafe, but
producer and consumer disagreeing about how many locals the frame has is the
shape of every other bug here.

### What is deliberately NOT checked

**"A bci with a non-zero dead mask must have a live entry."** It looks like an
invariant and is not one. The dead mask is filled from
`Compiler::osr_block_live_in` (basic-block starts); the entry table is filled
where the emitter decided an OSR entry is valid; the two sets are not equal on
the untransformed path. Asserting it would give a check that fails for a reason
unrelated to any defect — precisely how the earlier version of the loop
rewriter's OSR test "passed for the wrong reason and then failed for the wrong
reason" (`docs/jit/loop-rewriter-wiring.md`).

**"Every set bit in the published dead mask names a local the frame has."**
Checked and rejected 2026-08-04, because it is the closest thing to the brief's
"the three vectors are index-compatible" that spans into local-index space, and
a future reader will reach for it. It does **not** hold: `osr_local_assignments`
is the allocator's own vector and is legitimately *longer* than `num_locals` —
a category-2 high half pushes it past, which `osr_contract`'s
`assignment_vectors_must_cover_every_seeded_local` already asserts is fine — so
`resident`, and therefore the mask, can carry bits above `num_locals`. Those
bits are inert (both `validate_osr_entry` and `osr_trampoline` iterate
`0..num_locals` and never read them), so asserting it would drop OSR metadata on
correct methods and buy nothing. Same trap as the paragraph above, one space
over.

### Failing closed

On a violation the publication site publishes **no** OSR metadata at all rather
than a set whose pieces disagree. `can_osr_enter` then answers `false`
everywhere and the method runs to completion in the interpreter, which is always
valid. The lane's own rule: over-refusal costs an optimisation; under-refusal
re-runs loop iterations or resumes with the wrong locals.

The fields are *cleared*, not returned early: everything after that point in
`x64.rs` publishes non-OSR state (oop maps, deopt points, frame layout, the
epoch guard), and an early return would turn a metadata disagreement into a far
larger regression than the one it prevents.

`osr_contract_violations()` counts them, and the diagnostic is unconditional
rather than behind a debug flag — a compiler bug that silently costs OSR for a
method is exactly what a disabled-by-default diagnostic would hide.

### Measured: fail-closed costs nothing here

A fail-closed check is only free if it never fires, so that was measured rather
than assumed (Azure Linux, release binary, 2026-08-03):

| Workload | OSR entries | Contract violations |
|---|---|---|
| `ConfigurationPropertiesTests` (114 tests, all pass) | **0** | 0 |
| `apps/probes/OsrDeadLocalProbe` | **125** | **0** |

The first row is the reason the second exists. A Spring test class drives no OSR
at all, so "zero violations" there is vacuous — it says nothing about a check
that only runs on OSR-capable artifacts. `OsrDeadLocalProbe` is built to
coalesce a dead setup local and a live loop local onto one register, which is
exactly the `osr_dead_mask` shape this contract governs, and it produced 125
real entries.

The probe is differential, so the entries were also *correct*: its FNV-1a
accumulator is `5697627218349681645` on **HotSpot**, on **CratonVM `--nojit`**,
and on **CratonVM with the JIT and this change**. A check that refused OSR too
eagerly would have shown up as the JIT arm silently matching `--nojit` for the
wrong reason; 125 logged entries is what rules that out.

---

## Risks

1. ~~**A newtype for the two pc spaces is not there.**~~ Built — see step 5.
2. **The `unwrap_or(0)` in `can_osr_enter_with` is still there.** The contract
   check makes it unreachable at publication, not impossible in principle. Any
   future second producer of `osr_dead_mask` reintroduces the hazard unless it
   also goes through the check.
3. **Over-refusal is silent to the user.** A violation costs the method its OSR
   service and prints one line. `osr_contract_violations()` and
   `osr_coordinate_mismatches()` are what make that measurable, and as of
   2026-08-04 they are actually *readable*: `CRATONVM_DBG=jit-method-stats`
   prints both, alongside the per-door admission counts, the ungated-backend
   count, the bail-list short-circuits, the cap refusals and the stale-epoch
   refusals. Before that, every one of those accessors had no caller anywhere
   in the tree. The same hook now prints the **OSR lifecycle** row
   (`osr_entered` / `osr_exited` / `osr_refused_entry` / `osr_compile_declined`),
   which the `osr-02` lane ungated for the same reason and which nothing
   printed either. That row is the only place an *entry-time* over-refusal is
   visible at all: it shows up as `osr_entered` collapsing while
   `osr_refused_entry` rises, and nowhere else.
4. **The eager first-call door is dormant under the default configuration.**
   `bg-compile` is default-ON and reroutes a first-call compile to the
   background worker, so its gate is inert unless `CRATONVM_JIT=bg-compile=0`.
   The witness test has a second arm for exactly that reason: without it, that
   door could lose its gate entirely and both `admitted=0` and
   `ungated-backend-entries=0` would still read clean.

## What to refuse

An OSR entry for a bci whose frame state cannot be reconstructed exactly, a
metadata set whose pieces disagree, and one whose two views of the frame
disagree about which register file a local lives in. All three are the same
trade: over-refusal costs an optimisation, under-refusal re-runs loop iterations
or resumes with the wrong locals.

## See also

* `docs/jit/on-stack-replacement.md` — how entry itself works.
* `docs/jit/osr-vm-side-wiring.md` — the runtime half.
* `docs/jit/loop-rewriter-wiring.md` — the coordinate change under a bytecode
  transform, and the test that failed for the wrong reason.
* `docs/jit/loop-transform-osr-gap-and-switch-rule.md` — the unrolled back-edge
  gap the `-1` sentinel exists for.
