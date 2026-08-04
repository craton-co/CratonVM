# OSR entry metadata: the contract between the pieces

**Status: the metadata contract is executable and enforced; the second compile
door is still open and is now the whole remaining item.** OSR entry itself has
worked for a long time. What had no owner is the *contract between* the vectors
it rides on, and every near-miss this campaign found there was one bug class —
**a plausible integer in the wrong coordinate space**.

Answers the `osr-01` lane of `docs/known-issues/c2/deep-research-vm-c2.md`.

---

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
| 1 | `osr_pc_to_native` is bci-indexed by the runtime but output-pc-indexed by the emitter | **True, and handled** at the publication site (`x64.rs`, "Coordinate change, artifact half"). Not *typed* apart — see below. |
| 2 | The `-1` refusal sentinel was not enforced as an invariant | **Fixed before this lane.** The publication site re-imposes `-1` at every bci `osr_entry_pc` refuses, "regardless of which producer filled the vector". |
| 3 | The OSR compile path calls the backend directly, not through `try_compile` | **Still true.** `compile_osr_artifact` (`vm/src/runtime/interpreter/invoke.rs`) calls `x64::compile` directly and has since grown *hand-copied* gates — the permanent bail-list (RBC.2, after 35 923 wasted pipelines on `Nat.inc`) and the osr-entry-rejected memo (after 256 re-compiles over ten H2 operations). Each was added reactively, after its own bug. |
| 4 | `osr_entry_frame_state` and the deopt frame state are unchecked against each other | **Still true.** Not addressed here. |

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
| `probes/OsrDeadLocalProbe` | **125** | **0** |

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

## Implementation steps

### 1. The contract module · **DONE**

`jit/src/osr_contract.rs`: `check()` (pure, over slices), `check_at_publication()`
(counter + diagnostic), `OsrContractViolation` carrying the numbers rather than
just a discriminant — a violation is a compiler bug and the first question is
always "by how much". Six tests over synthetic vectors, including both
directions of the length mismatch and the "longer than `num_locals` is fine"
case (a category-2 high half can push the vector past it).

### 2. Wire it at publication · **DONE**

One call in `x64.rs` where the four vectors are finalised, fail-closed as above.

### 3. The remaining item: one door for "produce an OSR-capable artifact"

**Not done, and it is the largest of the four.** `compile_osr_artifact` calls
`x64::compile` directly; `try_compile` is a 25-argument entry point that does
substantially more. Unifying them is a real refactor, not a wrapper, and its
shape should be decided by what the *next* drift costs rather than by tidiness —
the two gates copied so far were each worth thousands of wasted pipelines, so
the cheap version is a **shared gate function both paths must call**, with a
test that the OSR path calls it. That is the next increment and it is a real
one.

### 4. Cross-check `osr_entry_frame_state` against the deopt frame state

Not done. Two views of one thing, unchecked. Lower value than step 3 while the
OSR frame state has no second producer to disagree with.

---

## Risks

1. **A newtype for the two pc spaces is not there.** The brief asks for one "or
   at minimum a debug assertion at each boundary". What exists is a checked
   *relationship* at the one publication site, which catches the consequence
   (vectors that disagree) rather than the cause (an integer in the wrong
   space). A `BciPc`/`OutPc` newtype across `x64.rs` and `lib.rs` remains the
   stronger answer and is unbuilt.
2. **The `unwrap_or(0)` in `can_osr_enter_with` is still there.** The contract
   check makes it unreachable at publication, not impossible in principle. Any
   future second producer of `osr_dead_mask` reintroduces the hazard unless it
   also goes through the check.
3. **Over-refusal is silent to the user.** A violation costs the method its OSR
   service and prints one line. `osr_contract_violations()` is what makes that
   measurable; it is expected to stay zero.

## What to refuse

An OSR entry for a bci whose frame state cannot be reconstructed exactly, and a
metadata set whose pieces disagree. Both are the same trade: over-refusal costs
an optimisation, under-refusal re-runs loop iterations or resumes with the wrong
locals.

## Effort

Steps 1–2: **done**. Step 3 (one door): **M**, and it is the one that keeps
paying. Step 4: **S**, low value until there is a second producer.

---

## See also

* `docs/jit/on-stack-replacement.md` — how entry itself works.
* `docs/jit/osr-vm-side-wiring.md` — the runtime half.
* `docs/jit/loop-rewriter-wiring.md` — the coordinate change under a bytecode
  transform, and the test that failed for the wrong reason.
* `docs/jit/loop-transform-osr-gap-and-switch-rule.md` — the unrolled back-edge
  gap the `-1` sentinel exists for.
