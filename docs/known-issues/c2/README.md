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
| HIR/LIR/MIR | ~~`hir-01`~~ **settled**, `hir-02` | The contract question is answered: `docs/jit/lowering-contract.md`. `hir-01` retired to `docs/internal/hir-01-lowering-contract-RETIRED-20260803.md` on 2026-08-03. `hir-02` is unblocked but should follow the contract's increment order, which starts with a step that emits nothing. |
| Profile-guided inlining | ~~`pgo-01`~~, ~~`pgo-02`~~ | Both lanes' first increments shipped 2026-08-03 — see `docs/feature-designs/profile-guided-inlining.md`. Monomorphic guarded virtual/interface inlining is real (behind `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`, default-off); Bimorphic and a deopt-capable guard remain open. |
| OSR | `osr-01`, `osr-02` | OSR entry works. Its metadata contract and its exit/recompile story are the gaps. |
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

`hir-01` and `verify-01` are the only two with a hard ordering claim: nothing
in the HIR lane should start before `hir-01` settles the contract, and every
other lane is easier to land once `verify-01` exists. The rest are
independent of each other by construction — that is what the ownership tables
are for.

**`hir-01` closed 2026-08-03.** Its answer is `docs/jit/lowering-contract.md`.
The one thing to carry into the other lanes: the contract's three-defect test
scored **one of three**, so the HIR/MIR migration is *not* justified as a
correctness investment, and its first increment emits no bytes and exists to
produce the measurement that decides whether to continue. Answering rule 1
("verify the premise") turned up three stale claims in neighbouring docs, two
of which asserted that finished work was unfinished — the failure mode this
directory's rule 1 was written for, in the direction nobody checks.
