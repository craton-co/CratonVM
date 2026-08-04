# The original lane briefs, recovered

**2026-08-03 — three files joined them on the opposite terms**, moved here whole
rather than deleted by the commit that landed them, because those lanes are
**finished** rather than first-incremented.

| file | retired by | what is genuinely finished | what is not |
|---|---|---|---|
| `cov-02-array-element-access.md` | closeout | all seven owned opcodes, plus `laload`/`saload`/`lastore`/`castore`/`sastore` for symmetry. Measured refusals for the seven 79 → **0**; optimizing-backend bodies 591 → **652** across the survey's three Spring Boot workloads | `aastore` (0x53), declined on purpose — it needs the SATB + card write barriers, and the refusal is written into the builder. Two measured events. `dup2` (0x5c), one event, no owner |
| `meas-02-the-bench-suite-does-not-reach-c2.md` | closeout | all three of its asks: the gate records per-phase C2 reach, the survey has a C2-reach column, and one candidate (`bench/CratonBenchC2.java`) is characterised. It also found the gate could not compile its own benchmark under its own `LC_ALL=C` | the candidate is not anchored — deliberately, and blocked on a quiet host and on an unexplained 17x run-to-run swing, not on a decision |
| `cov-03-field-stores-and-wide-fields.md` | closeout | both its sites measured to zero on the tree it landed on — builder refusals **72 → 2**, bodies **588 → 660**, the two survivors `cov-06`'s. One shared tag classifier so the two arms cannot drift apart again. The brief's open question ("which of the two candidate reasons is the real one?") is answered: the **write barrier**, and the compact-layout candidate was not a reason at all. Its own banner's "re-measure, this lane is bigger than 43" was right — it was 70 | no barrier-free fast path for a C2 reference store, no inline route for a wide field read, and — the one that outlives this lane — **no measurement that an optimizing body is FASTER than the single-pass one it replaces**, for any `cov-*` lane |

Read the closeouts; these copies of the briefs are here so their reasoning
stays readable, not because anything in them is still open.

## The other nine

These nine files are the lane docs this directory shipped with. Each was
**deleted by the commit that implemented its first increment**, so the brief
went away at the moment the work stopped being hypothetical — and with it the
list of what the lane had *not* done. That is the failure this directory exists
to fix, and it is why the three lanes above MOVED their briefs here instead.

They are here unchanged. Nothing in this directory should be planned from them
without checking the parent `README.md` first: their "Current state" sections
describe 2026-08-01, and at least one lane discovered its subject already
existed.

| file | retired by | what is genuinely finished | what is not |
|---|---|---|---|
| `seam-01-x64-backend-split.md` | `60e297ec9` | all of it — `x64.rs` 40,588 → 2,539 lines | — |
| `seam-02-invoke-dispatch-split.md` | `44aabd78b` | all of it — the interpreter's two files 26,775 → 8,158 and 24,817 → 3,941 | — |
| `hir-01-lowering-contract.md` | `a628e18cb` | the contract is settled; four levels, not three | — |
| ~~`hir-02-mir-regalloc-handoff.md`~~ **RETIRED 2026-08-04** | `d0ad74f1a`, then `fix/hir02-mir-regalloc-handoff-20260804` | the selector has a production caller (`CRATONVM_JIT=ir-isel-emit`, default off, fail-closed) with a byte-equality oracle that found 0 disagreements over a real workload; the `lea_r32_m` row landed and `Rule::Lea` fires 26 tiles where it fired none; the vector-pool hazard is the caller's to state | emitting the rules byte equality cannot cover, and increment 3 — both with their reasons in `hir-02-mir-regalloc-handoff-RETIRED-20260804.md` (internal tree) |
| `pgo-01-call-site-evidence-gap.md` | `e183ccc41` | `invokestatic`/`invokespecial` feed `MethodProfile::call_sites` | — |
| `pgo-02-guarded-inlining.md` | `76f4ca274`, residuals 2026-08-04 | monomorphic guarded virtual/interface inlining, default-off; then bimorphic splicing, the `StableType` index, the tally denominator, the saturated-profile refusal, and a fix for the shipped increment's WRONG-CODE bug (the guard named one class, the spliced body came from another) | a deopt-capable guard, blocked on `deopt::FrameState::caller` being populated — now refused mechanically rather than by convention. See `../../internal/pgo-02-guarded-inlining-RETIRED-20260804.md` |
| ~~`osr-01-entry-metadata-contract.md`~~ — **moved out 2026-08-04** to `osr-01-entry-metadata-contract-RETIRED-20260804.md` | `c87c65f08`, then finished 2026-08-04 | **all of it**: the metadata contract, the pc-space newtype (`jit/src/osr_coords.rs`), one compile door for all three (`jit/src/compile_gate.rs`) and the frame-view cross-check (`CompiledMethod::osr_home_disagreement`) | — |
| `osr-02-exit-and-recompile.md` | `4bec5efca` | the per-pc memo already existed; lifecycle counters landed | the exit-state differential, which is the increment the doc called the point of the lane |
| `verify-01-differential-harness.md` | `923610c81` | `scripts/verify/compare.py`, fixture checks, H2/Tomcat/Spring Boot baselines | the doc's own note that "every lane is easier to land once `verify-01` exists" still stands, and the `cov-*` lanes are the first ones to test that |

## Why this directory exists at all

Retiring a brief when its first increment lands is reasonable — a stale brief
is worse than none, and this project has been bitten by exactly that (the HIR
closeout found five stale claims, four of them asserting that *finished* work
was unfinished). What it costs is the other direction: the residuals move into
one cell of a summary table in the parent README and stop reading like work.

Four of the nine rows above had a live residual. Two of those — the isel pattern
rows and the OSR second door — were named in the parent README and owned by
nobody. That is the failure this archive is meant to make visible, and the
`cov-*` lanes next to it are what a lane doc should look like while its work is
still open.

**It worked, three times over.** All three of those unowned residuals — the isel
pattern rows, `pgo-02`'s bimorphic splicing, and the OSR second door — were
picked up and closed on 2026-08-04, each *from its row in this table*, and the
row was where the residual was legible enough to be picked up at all. What the
OSR lane then found is the argument
for keeping the archive rather than for trusting it: its brief's four items had
been triaged into "one remaining", and finishing them turned up **three** compile
doors where the brief said two, a code-cache cap neither direct door had ever
checked, a compile-epoch witness a thousand lines out of place, and four
diagnostic counters with no caller anywhere in the tree. A residual summarised
into one cell is a pointer, not an inventory.

## Recovering one

```bash
git show <retiring-sha>^:docs/known-issues/c2/<file>
```

The shas are in the table. `git log --all --diff-filter=D -- docs/known-issues/c2/`
lists every deletion in this directory, including the fourteen unrelated docs
moved out by `af0abc887` (they went to `docs/feature-designs/`, they were not
retired).
