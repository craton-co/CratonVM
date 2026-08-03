# C2 review — the lanes that were NOT implemented

`docs/known-issues/c2/deep-research-vm-c2.md` was worked through by two waves of
parallel agent lanes. Its decomposed, file-level items landed. Five lanes did
not, and this directory is those five, decomposed into work that can run in
parallel.

Read the report's remediation banner first. It records what landed, which of
the report's premises turned out to be false, and — the part that matters
here — which lanes have a **first increment** rather than a finished lane.

## The five untouched lanes

| Lane | Docs | Why it is untouched |
|---|---|---|
| HIR/LIR/MIR | ~~`hir-01`~~ ~~`hir-02`~~ **both closed 2026-08-03** | Consolidated into `docs/feature-designs/jit-machine-level-and-instruction-selection.md`. Four levels, not three; the report's "HIR" is the bytecode. Increment 0 (shadow selection, emits nothing) landed and measured **15.7–19.0%** coverage on real compiles with `Rule::Lea`/`AluImm` firing **zero** times — so the next step is six 32-bit pattern rows, not a machine level. |
| Profile-guided inlining | ~~`pgo-01`~~, `pgo-02` | `pgo-01`'s first increment shipped 2026-08-03 — see `docs/internal/pgo-01-call-site-evidence-gap-RETIRED-20260803.md`. The *policy* still is not fed: `pgo-02`'s speculation has no guard/deopt pairing yet. |
| OSR | ~~`osr-01`~~ **closed 2026-08-03**, `osr-02` | The metadata contract is executable and enforced — `docs/feature-designs/jit-osr-entry-metadata.md`. Two findings: there are **three** coordinate spaces, not the two the brief names, and the second compile door (`compile_osr_artifact` calling `x64::compile` directly) is still open and is now the whole remaining item. `osr-02`, the exit/recompile story, is untouched. |
| Loop transforms | `loop-01`, `loop-02` | One transform (bytecode unroll) is wired behind an opt-in. Everything else is unbuilt, and the planner refuses most compiles for reasons nobody has revisited. |
| `x64.rs` / `invoke.rs` seams | `seam-01`, `seam-02` | 40k and 24k lines. The split is mechanical but every lane in the wave collided on these two files. |

Plus `verify-01`, which is not a lane — it is the harness every lane above
needs in order to prove it did not regress anything. Its first increment
shipped 2026-08-03 (`scripts/verify/compare.py` + fixture checks in the H2
and Tomcat runners + real checked-in baselines for H2/Tomcat/Spring Boot) —
see `docs/internal/verify-01-differential-harness-RETIRED-20260803.md`.

## Rules that made the last two waves work

These are not style preferences. Each one is a defect this campaign actually
shipped or narrowly avoided.

1. **Verify the premise before implementing.** Roughly one premise in three in
   the original report did not hold. Three separate lanes discovered their
   subject already existed, had never been compiled, or that the module's own
   doc made a false claim that was hiding the real hazard. Report the delta
   before writing code.
2. **Disjoint file ownership.** Each doc below names the files its lane owns.
   Two lanes editing one file is how a wave loses work.
3. **Fail closed.** Refusing to compile a method beats emitting plausible
   wrong code. This VM has shipped multiple silent-corruption bugs from the
   other choice, and every one of them surfaced far from its cause.
4. **A new behaviour lands off by default**, behind a *declared* flag. An
   undeclared flag is invisible to `-XX:` and to the test override mechanism,
   and the declaration sweep has already had to be re-run once because a flag
   was added 21 minutes after it closed at zero.
5. **A test that cannot fail is worse than no test.** Two examples from the
   wave: an assertion that passed vacuously because the registry it inspected
   was empty, and a premise check that "proved" a transform had fired when it
   was actually observing an unrelated switch. Write down the exact edit that
   would trip each new check.
6. **Do not trust a handover's census.** The JVMTI lane's handover said ~15
   call sites in one file; the real count was 28 across two. Re-derive counts.

## Sequencing

`hir-01` and `verify-01` were the only two with a hard ordering claim: nothing
in the HIR lane should start before `hir-01` settles the contract, and every
other lane is easier to land once `verify-01` exists. The rest are
independent of each other by construction — that is what the ownership tables
are for. (The HIR ordering claim is discharged; `verify-01` still stands.)

**The HIR lane closed 2026-08-03**, both docs, into
`docs/feature-designs/jit-machine-level-and-instruction-selection.md`. Three
things to carry into the other lanes:

1. The three-defect test scored **one of three**, so that migration was never
   justified as a correctness investment — only by a measurement.
2. The measurement then said **stop**: 15.7–19.0% coverage on real compiles,
   with the two rules the migration was *for* firing zero times. A ten-shape
   synthetic corpus had said 38.2% and named the wrong rules. **Do not size a
   lane from a fixture's node mix.**
3. Rule 1 ("verify the premise") turned up *five* stale claims, four of which
   asserted that finished work was unfinished — the failure mode this
   directory's rule 1 was written for, in the direction nobody checks. One was a
   red test on `dev` that predated the lane entirely.
