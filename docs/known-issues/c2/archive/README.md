# The original lane briefs, recovered

These are the lane docs this directory shipped with. Each was **deleted by the
commit that implemented its first increment**, so the brief went away at the
moment the work stopped being hypothetical — and with it the list of what the
lane had *not* done. (`meas-02`, 2026-08-03, was *moved* here rather than
deleted and recovered. Same effect, one less archaeology step.)

They are restored here unchanged. Nothing in this directory should be planned
from them without checking the parent `README.md` first: their "Current state"
sections describe 2026-08-01, and at least one lane discovered its subject
already existed.

| file | retired by | what is genuinely finished | what is not |
|---|---|---|---|
| `seam-01-x64-backend-split.md` | `60e297ec9` | all of it — `x64.rs` 40,588 → 2,539 lines | — |
| `seam-02-invoke-dispatch-split.md` | `44aabd78b` | all of it — the interpreter's two files 26,775 → 8,158 and 24,817 → 3,941 | — |
| `hir-01-lowering-contract.md` | `a628e18cb` | the contract is settled; four levels, not three | — |
| `hir-02-mir-regalloc-handoff.md` | `d0ad74f1a` | the shadow selection pass exists and was measured | isel covers 15.7–19.0% on real code and `Rule::Lea`/`AluImm` fire zero times — six 32-bit pattern rows are the named next step, and nobody owns them |
| `pgo-01-call-site-evidence-gap.md` | `e183ccc41` | `invokestatic`/`invokespecial` feed `MethodProfile::call_sites` | — |
| `pgo-02-guarded-inlining.md` | `76f4ca274` | monomorphic guarded virtual/interface inlining, default-off | bimorphic splicing, a deopt-capable guard, `StableType` invalidation, the metrics harvest — all still listed open in `docs/feature-designs/profile-guided-inlining.md` §8 |
| `osr-01-entry-metadata-contract.md` | `c87c65f08` | the metadata contract is executable and fails closed | the **second compile door** — `compile_osr_artifact` calls `x64::compile` directly — is untouched and is the whole remaining item |
| `osr-02-exit-and-recompile.md` | `4bec5efca` | the per-pc memo already existed; lifecycle counters landed | the exit-state differential, which is the increment the doc called the point of the lane |
| `verify-01-differential-harness.md` | `923610c81` | `scripts/verify/compare.py`, fixture checks, H2/Tomcat/Spring Boot baselines | the doc's own note that "every lane is easier to land once `verify-01` exists" still stands, and the `cov-*` lanes are the first ones to test that |
| `meas-02-the-bench-suite-does-not-reach-c2.md` | `docs/internal/meas-02-bench-suite-c2-reach-RETIRED-20260803.md` | all three of its asks: the gate records per-phase C2 reach, the survey has a C2-reach column, and one candidate (`bench/CratonBenchC2.java`) is characterised. It also found the gate could not compile its own benchmark under its own `LC_ALL=C` | the candidate is not anchored — deliberately, and blocked on a quiet host rather than on a decision |

## Why this directory exists at all

Retiring a brief when its first increment lands is reasonable — a stale brief
is worse than none, and this project has been bitten by exactly that (the HIR
closeout found five stale claims, four of them asserting that *finished* work
was unfinished). What it costs is the other direction: the residuals move into
one cell of a summary table in the parent README and stop reading like work.

Four of the nine rows above have a live residual. Two of those — the isel
pattern rows and the OSR second door — are named in the parent README and owned
by nobody. That is the failure this archive is meant to make visible, and the
`cov-*` lanes next to it are what a lane doc should look like while its work is
still open.

## Recovering one

```bash
git show <retiring-sha>^:docs/known-issues/c2/<file>
```

The shas are in the table. `git log --all --diff-filter=D -- docs/known-issues/c2/`
lists every deletion in this directory, including the fourteen unrelated docs
moved out by `af0abc887` (they went to `docs/feature-designs/`, they were not
retired).
