# OSR entry metadata: the contract between the pieces

**Status: CLOSED 2026-08-04. All four of the brief's near-misses are handled,
both remaining implementation steps landed, and Risk 1 (the newtype) is built.**
OSR entry itself has worked for a long time. What had no owner is the *contract
between* the vectors it rides on, and every near-miss this campaign found there
was one bug class — **a plausible integer in the wrong coordinate space**.

Answers the `osr-01` lane of `docs/known-issues/c2/deep-research-vm-c2.md`. The
brief is retired to
`osr-01-entry-metadata-contract-RETIRED-20260804.md`.

## What the closing increment found

Three things worth carrying into a neighbouring lane, each of which contradicted
something this document previously said or assumed:

1. **There are THREE compile doors, not the two the brief names.** `try_compile`,
   `compile_osr_artifact`, and the interpreter's *eager first-call* compile in
   `execute`. `jit_force_interpret`'s own doc had already enumerated all three
   from the other side and nothing connected the two observations. Both direct
   doors had hand-copied *subsets* of the admission chain and **neither had ever
   checked the code-cache cap**.
2. **The OSR door's compile-epoch witness was ~1,000 lines too late.** It was
   opened immediately before the backend call, after all of that function's
   class loading and constant-pool resolution — so a redefinition landing in
   that window produced a body stamped with the *current* epoch, which the
   install barrier then accepted. `try_compile` opens its witness "FIRST, before
   any constant-pool resolver runs" and says so in a comment; the OSR copy of
   that comment claimed the same thing and was in the wrong place.
3. **Every counter this document cited as evidence had no caller.**
   `osr_contract_violations()`, `jit_bail_shortcircuits()`,
   `jit_code_cache_cap_refusals()` and `stale_install_epoch_refusals()` are all
   `pub fn`s that nothing in the tree read. Each one's doc says it is "expected
   to stay zero" and that a diagnostic nobody enables is how a compiler bug
   stays unnoticed — and none of them could be enabled at all. They now print
   under `CRATONVM_DBG=jit-method-stats`.

And one about the *test*, which is the more transferable finding: the door
witness was first written under `vm/tests/`, and its first injection run — the
gate deleted outright from the OSR door — **passed**. `cargo test -p cratonvm-vm`
does not rebuild the `cratonvm` binary, so the probe ran a binary from several
commits earlier. Any test in this repository that spawns `target/release/cratonvm`
has that hazard. The fix is to host it in `vm-cli`, whose own integration tests
get `CARGO_BIN_EXE_cratonvm` pointed at the binary cargo just built.

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
| 1 | `osr_pc_to_native` is bci-indexed by the runtime but output-pc-indexed by the emitter | **Handled and now TYPED apart.** `jit/src/osr_coords.rs` — `OutPcIndexed` / `BciIndexed`, with the identity conversion checked against `orig_code_len`. |
| 2 | The `-1` refusal sentinel was not enforced as an invariant | **Fixed before this lane.** The publication site re-imposes `-1` at every bci `osr_entry_pc` refuses, "regardless of which producer filled the vector". |
| 3 | The OSR compile path calls the backend directly, not through `try_compile` | **Fixed 2026-08-04.** `jit/src/compile_gate.rs` is the one door; all three call it, and a backend entry with no admission open is counted. |
| 4 | `osr_entry_frame_state` and the deopt frame state are unchecked against each other | **Fixed 2026-08-04.** `CompiledMethod::osr_home_disagreement`, consulted by `validate_osr_entry` before the per-slot loop. |

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

### 3. One door for "produce an OSR-capable artifact" · **DONE 2026-08-04**

`jit/src/compile_gate.rs`. The shape this document prescribed — "a shared gate
function both paths must call, with a test that the OSR path calls it" — with
one correction and one strengthening.

The correction: **there are three doors**, and the third (the interpreter's
eager first-call compile) is gated too. Leaving it out would have made the drift
witness below meaningless, because it would have been counting a real door as a
violation forever.

`compile_gate::admit(class, method, descriptor, door)` asks four questions and
does two things:

| | Was asked by | Now asked by |
|---|---|---|
| `CRATONVM_DISABLE_JIT` | OSR + the VM-side callers | all three |
| the permanent bail-list | method-entry; **hand-copied** into OSR | all three |
| `CRATONVM_JIT_DENY` / `_BISECT_ONLY` | method-entry; **hand-copied** into OSR and eager-first-call | all three |
| the code-cache cap | method-entry **only** | all three |
| opens the compile-epoch witness | method-entry, first; OSR, ~1,000 lines late | all three, first |
| clears the one-shot bail-site record | method-entry | all three |

It returns a `#[must_use]` RAII token that owns the witness, so dropping it
early re-narrows the window it exists to widen.

**The drift witness.** `x64::compile_with_param_slots` calls
`compile_gate::note_backend_entry`, which counts backend entries taken with no
token open on the thread. A fourth door does not merely miss the checks; it
moves `ungated_backend_entries()`, which is asserted zero over a real run.
Behaviour-named rather than source-scanning, because five checks in this
repository named a *file* where they meant a module and died when that file was
split.

Two things this deliberately did NOT do. `compile_osr_artifact` asks the two
whole-method vetoes (kill switch, bisect levers) *before* consulting its
artifact cache, via `compile_gate::compiled_execution_forbidden`, and takes the
full admission only when it is about to compile — refusing to *reuse* a body
that is already committed because the code cache is full would cost throughput
and buy nothing. And `SELF_CALL_IDENTITY_STABLE` stays in `try_compile`: it is a
proof the caller deposits for one compile, and the other two doors neither set
nor read it.

### 4. Cross-check `osr_entry_frame_state` against the deopt frame state · **DONE 2026-08-04**

`CompiledMethod::osr_home_disagreement`, consulted by `validate_osr_entry`
before its per-slot loop.

The two views are not "the entry frame state and the deopt frame state" — those
are literally the same object; `osr_entry_frame_state` reads `deopt_points`. The
real pair, and the one that can contradict, is:

* the **precise `FrameState`** at the entry bci — what `validate_osr_entry`
  type-checks the interpreter's offer against; and
* the **register homes** (`osr_local_assignments` / `osr_xmm_assignments`) —
  what `osr_trampoline` actually seeds through.

Both are produced by one compile from one allocator state, so a disagreement
means *the entry that was validated is not the entry that is performed*: a
`double` whose only home is a GPR has its bits moved into a register the body
reads as an integer, and — worse — a reference whose only home is an XMM is
seeded into the FP file **with the frame-slot store elided**, so the GC's
precise map has nothing to find.

Deliberately narrow, and the narrowness is the design:

* it cross-checks the register **file** only. It does not assert that a live
  slot has a home (memory-homed locals are ordinary), that a homed slot is
  described (a snapshot shorter than the frame simply does not describe its
  tail), or anything about the dead mask;
* a slot's homes are an *admissible set*, not a single answer. A JVM slot index
  is legally reused by locals of different types in disjoint live ranges
  (`DualPivotQuicksort.mixedInsertionSort` has slot 7 as a `long`'s high half in
  one region and an `int` counter in the others), so one slot can carry BOTH a
  GPR and an XMM home and the trampoline seeds both. Treating "has an XMM home"
  as "is FP" would have refused those;
* masked-dead slots are skipped: the trampoline does not seed them, so their
  homes describe nothing that happens at this entry.

The refusal tag `osr-entry-contract-disagreement` is **artifact-level** and
therefore memoable through `mark_osr_entry_rejected` — it reads no offered
locals. Memoing a state-dependent refusal is the same bug in the other
direction: a silent, permanent loss of OSR.

### 5. The pc-space newtype (was Risk 1) · **DONE 2026-08-04**

`jit/src/osr_coords.rs`: `OutPcIndexed<T>` and `BciIndexed<T>`. Not a
`BciPc`/`OutPc` scalar newtype threaded through `x64.rs` and `lib.rs` — that
would have rippled into the artifact corpus, the runtime and thirty test call
sites for a property that only matters at one boundary. What the *vectors* are
indexed by is the thing that goes wrong, so the vectors carry the type.

There are exactly two ways out of output-pc space, and the dangerous one is the
boring one:

* `BciIndexed::from_translated` — the loop rewriter is armed, so
  `rebuild_pc_to_native` (or the pointwise `osr_entry_pc` remap) produced a new
  vector. Announced by its own call.
* `OutPcIndexed::into_bci_by_identity` — the rewriter is not armed, so the two
  spaces coincide and the vector is reinterpreted. **This was a bare
  `None => osr_entry_native` match arm.** The assumption behind it —
  `code_len == orig_code_len` on that path — was stated nowhere and checked
  nowhere. The conversion now takes `orig_code_len` and verifies the length it
  implies, and a mismatch takes the same fail-closed path as a contract
  violation.

`osr_coordinate_mismatches()` counts them; `coordinates_agree` short-circuits
the contract check so one bug is not counted in both places.

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
   in the tree.
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

## Verification (2026-08-04, Azure Linux, release build)

Every check here is fail-closed, so "it never fires" has to be a measurement and
each one has to be shown capable of firing.

**Correctness, differential.** `probes/OsrDeadLocalProbe` FNV-1a accumulator
`5697627218349681645` on **HotSpot (JDK 21)**, on **CratonVM `--nojit`**, and on
**CratonVM with the JIT and this change** — the same value this document
recorded for the previous increment. `bench/CratonBench`: all seven phase
checksums identical to HotSpot, in both the default arm and the
`bg-compile=0` arm.

**The counters, on real runs.**

| Workload | method-entry | eager-first-call | osr | ungated | contract viol. | coord. mismatch |
|---|---:|---:|---:|---:|---:|---:|
| `OsrDeadLocalProbe` | 0 | 0 | **8** | 0 | 0 | 0 |
| `CratonBench`, default | 8 | 0 | **7** | 0 | 0 | 0 |
| `CratonBench`, `bg-compile=0` | 8 | **22** | 7 | 0 | 0 | 0 |

The first row is why the third exists: no single configuration exercises all
three doors, so a one-arm measurement would have left two of them unproven.

**Each check shown capable of firing** (inject, build, run, revert):

| Injected edit | Observed |
|---|---|
| delete `compile_gate::admit` from `compile_osr_artifact` | `osr: admitted=0`, `ungated-backend-entries=3`; the witness test FAILS |
| delete it from the eager first-call door | `eager-first-call: admitted=0`, `ungated-backend-entries=9`; the witness test FAILS |
| `into_bci_by_identity(orig_code_len + 1)` at the publication site | `osr-coordinate-mismatches=8` with a per-method line naming both lengths; `osr-contract-violations` stays **0** (the short-circuit stops one bug being counted twice); the probe's FNV accumulator is **unchanged** — the fail-closed path drops OSR and the interpreter finishes the method, which is what fail-closed is supposed to look like |

And the injection that mattered most: the *first* run of the first injection
**passed**, because the test then lived under `vm/tests/` and cargo had not
rebuilt the binary it spawned. See the banner at the top of this document.

**Suites.** `cargo test --release -p cratonvm-jit`: 1,912 lib tests + all
integration targets, 0 failed.

## Effort

Steps 1–5: **done**. Nothing in this document is open.

---

## See also

* `docs/jit/on-stack-replacement.md` — how entry itself works.
* `docs/jit/osr-vm-side-wiring.md` — the runtime half.
* `docs/jit/loop-rewriter-wiring.md` — the coordinate change under a bytecode
  transform, and the test that failed for the wrong reason.
* `docs/jit/loop-transform-osr-gap-and-switch-rule.md` — the unrolled back-edge
  gap the `-1` sentinel exists for.
