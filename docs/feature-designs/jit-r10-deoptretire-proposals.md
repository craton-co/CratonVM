# Round 10 wave 9, lane `deoptretire` — proposals

**Lane:** round 10 wave 9, `deoptretire`.
**Owned files:** `jit/src/{lib,deopt,ir,ir_lower,ir_verify,osr_exit,metrics}.rs`,
`jit/src/x64/{driver,inlining,loop_rewrite,deopt_stubs}.rs`, `jit/src/tests.rs`,
`jit/tests/r10_{offsetkey,interner}_*.rs`,
`vm/src/runtime/interpreter/{deopt_resume,jit_bridge,tests}.rs`,
`vm/src/runtime/deopt_materialize.rs`, `vm/src/jit/conservative_roots.rs`,
the three `deoptverify`/`interner` pages and the design docs they cite.

**This lane BUILT AND TESTED**, unlike waves 6–8 on the same files. Everything
below is either a landed change with a test behind it or a decision stated with
its reason; nothing here is a reading that was never compiled.

Its brief was three known-issues pages and everything they carried forward. All
three are retired to `docs/internal/fixed-bugs/`. What follows is the part worth
keeping: the three things this lane deliberately did **not** do, and why each
refusal is a decision rather than an omission.

---

## 1. `RESUME` points are still refused by the ordinary stash, deliberately

`docs/jit/deopt-frame-state-interning.md` §5.2 asked for a consumer that reads
`point.semantics`, so that "a producer that knows better — a caller scope parked
mid-`invoke`, or a genuine post-call resume point — can say so in a way that
changes what the interpreter does". Every routing and resume decision now reads
the field. One thing still does not follow from it: a `RESUME` point in the
ORDINARY sink is replaced by the re-run sentinel rather than parked after its
bci, even though the VM can compute the successor (`caller_resume_pc` does it
for every caller scope of a chain).

**That is not the missing half of §5.2; it is a different change with a real
argument against it.** `deopt::deopt_stash_nesting_counts`' doc derives a
property the whole stash design rests on: a frame UNDER the top of `LAST_DEOPT`
can only be a stale leftover, never a live callee. The derivation's first step is
*"only a re-execute point may enter, so a trap at an invoke bci means the invoke
has not run"*. Admitting a resume-AFTER point makes that false, and the
consequence is not a refused frame — it is `pop_stash` discarding what it now has
to treat as a chain, or a consumer resuming a frame whose callee is still live.

So the order of work, if a producer of post-call resume points ever appears:
answer the nesting argument first (either the stash becomes a real chain, or such
points get their own stash the way `PendingException` frames did), then honour
the semantics. Not the other way round. The refusal is counted
(`deopt_stash_non_reexecute_refused`) and traced, so it cannot happen quietly.

## 2. The callee-local WIDTH source stays unwritten

Carried from `docs/feature-designs/jit-r10-splice-proposals.md` §1 by way of the
by-bci page. A deopt point published from inside a splice describes the callee's
geometry with every slot `FrameValue::Unsupported`, which is honest and
fail-closed and buys no reach: one `Unsupported` slot makes the frame
unresumable. Two of the three inputs for precise slots exist (oop-ness via
`Compiler::inline_oop_scopes`, homes at `local_base + i*8`); what is missing is a
width and a liveness source for the callee.

**Re-checked this wave and still refused, for the reason §1 of that document
gives about itself:** with no publisher inside a splice, a callee kind table
would be an analysis whose result nothing reads — which is precisely the defect
class this round exists to stop growing, and it would be *this lane* adding one
while retiring two others. The order is publisher first, with a workload showing
the reach it buys, then the analysis.

The measurement backing that stays honest, and how it was taken, because the
obvious way does not work: **`regression-suite/run.sh` does not surface VM
stderr** — it captures each vector's output into a variable and prints only what
`extract()` matches — so running the whole suite under `CRATONVM_DBG_EXCFRAME=1`
and grepping its log measures nothing, and reads as a clean zero while doing so.
(This lane tried it first and got exactly that: zero diagnostic lines of ANY
kind, including the `[cratonvm-jitc]` ones the same run must have produced by the
thousand.)

Taken directly instead, on the release binary built from this tree (merged with
`dev`), over the 17 JIT- and exception-heavy vectors (`RJit*`, `RExceptions`,
`RThrowableFillFrames`, `RLoaderExceptionShape`) run as
`cratonvm --java-home ... -cp regression-suite/build <Class>` with
`CRATONVM_DBG_EXCFRAME=1 CRATONVM_DBG_JITC=1`:

| signal | count |
|---|---|
| `[cratonvm-jitc]` lines (the control — the flags are live) | 107 230 |
| `SPLICE FRAME` | **0** |
| `BY-BCI FILING REFUSED` | **0** |
| `deopt-metadata-violation` | **0** |

So the branch that would consume a callee width table is unreached, wave 8's
by-bci guard refuses nothing, and the armed `DeoptVerifier` discards no artifact
— the three premises this round's deopt work rests on, checked rather than
assumed. The full suite is a separate, clean run: **102 passed, 0 failed** on the
merged tree (101 on the pre-merge one; `dev` added `RJitSyncBlockScalar`).

## 3. `check-orphan-instruments.sh` and `&self` readers

`docs/feature-designs/jit-r10-interner-proposals.md` §5 is the one item of that
document left open. This lane is the second data point for it and has nothing to
add to the proposal, but the second data point is worth recording because it is
*stronger* than the first:

* the interner (wave 7) was invisible to the gate because every `InterningStats`
  getter is an `&self` method with no arguments — check C2 excludes those by
  design;
* `ir::InlineScopeTable` (this wave) was invisible for a different and worse
  reason: **it had production callers.** `ir_lower::caller_chain_for`,
  `resolve_frame_state` and `lower_inner_with_scopes` all referenced it from
  non-test code, so no reference-counting gate could ever have flagged it. The
  thing with no reader was the WRITE side, three methods deep inside a type whose
  read side was live.

That is a different shape from the one the gate hunts, and it is the one that
survived longest: a `rg` for the interner told the truth, while a `rg` for
`InlineScopeTable` reported inlined-scope support the optimizing tier has never
had. A gate that counts references cannot see it; what would is a rule about
**types whose mutating methods have no production caller** — a table nothing ever
writes is empty on every run, whatever reads it. Offered as a second rule
alongside §5's, not as a replacement.

---

## What this lane ran, exactly

* `cargo check --workspace --all-targets` — clean.
* `cargo test -p cratonvm-jit --lib` — 3 375 passed, 0 failed.
* `cargo test -p cratonvm-jit --tests` — 63 test binaries, all green.
* `cargo test -p cratonvm-vm --lib --all-features` — 4 522 passed, 0 failed.
* `cargo test --workspace --tests --no-fail-fast` — the failures it reports are
  all outside this lane's diff, which touches only `jit/` and `vm/`:
  `native-builtins`' stub ratchet, two `native-collections` tests,
  `cratonvm-types`' doc-citation and LOC-table gates (all three drifts are
  *upward*, i.e. this lane's ~2 400 deleted lines moved them toward their claims,
  not away), and the undeclared `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT` flag
  introduced by `1354647ae` on `dev`. Two `cratonvm-vm` tests
  (`the_production_quiescence_signal_is_the_global_jit_depth`,
  `t19_6_wake_dedupes_concurrent_calls`) failed only in the fully parallel
  workspace run and pass 3/3 in isolation and under `--all-features`; both are
  timing-sensitive and neither is in a file this lane touched.
* `rustfmt --edition 2021 --check` on every edited file. The pre-existing drift
  the wave-7 lane recorded in `jit/src/ir_lower.rs` (one call site at
  `emit_inline_callee_deopt_service`) is still there and still not this lane's.
* A release build (fat LTO) under `CARGO_TARGET_DIR=target-deoptretire`, copied
  to `cratonvm-jitr10-w9.exe`, and `regression-suite/run.sh` on it:
  **102 passed, 0 failed**, 0 list/coverage errors, 0 harness-blindness flags.
  Note for the next lane: pass `JDK=$(cygpath -m ...)`. A POSIX `JDK=` path makes
  the VM reject the image and all 101 vectors fail as HARNESS FAULT, which the
  runner does diagnose — but only after printing a 0/101 tally that looks like a
  catastrophic regression.
* The diagnostic probe in §2, direct rather than through `run.sh`, for the reason
  given there.
