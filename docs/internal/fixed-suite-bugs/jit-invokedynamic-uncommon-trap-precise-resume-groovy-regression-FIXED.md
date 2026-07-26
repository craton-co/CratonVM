# JIT invokedynamic uncommon-trap precise-resume regressed Groovy dynamic dispatch

| | |
|---|---|
| **Status** | FIXED (2026-07-07). Reason 8 (`UnreachedCode`, the invokedynamic uncommon trap) keeps `fb4a333d`'s unconditional precise-resume routing — no tradeoff, no reopened corruption risk. Root cause was three separate, real bugs in the surrounding deopt/de-speculation machinery (NOT the operand-stack soundness gap originally suspected), all now fixed. Groovy matches the pre-existing baseline exactly (30/36 isolated, 5/36 batch — both numbers identical with or without this fix, confirming the residual gap is pre-existing and unrelated); the standalone corruption-repro suite is now provably correct against HotSpot ground truth where it previously crashed. |
| **Area** | JIT x86-64 backend — `invokedynamic` (`0xba`) uncommon-trap deopt/resume (`../../../jit/src/x64.rs`), `getstatic` (`0xb2`) reference-typed oop-marking (`../../../jit/src/x64.rs`), and the deopt-resume/de-speculation consumers in `../../../vm/src/runtime/interpreter.rs`. |
| **Symptom (original)** | With the JIT enabled (default), most methods of Spring's `org.springframework.context.groovy.GroovyBeanDefinitionReaderTests` failed with a Groovy-COMPILER-internal error (`startup failed: ... duplicates another method of the same signature` / `is a duplicate of the one declared for this script's body code` / bare `Should never happen`), reproducing in total isolation, disappearing under `--nojit`. |
| **Symptom (separately found while investigating)** | The standalone corruption repros this doc's `fb4a333d` predecessor was meant to fix (`AccumRepro3`, `LicmRepro`, `LicmRepro2`, `LicmRepro3`, `ArrRepro`) were never actually committed to the repository; recovered from an uncommitted sibling worktree this session. Running them exposed a SEPARATE, real bug: `getstatic`'s JIT codegen never marked a reference-typed static field's pushed value as a GC/deopt oop, corrupting `LicmRepro`/`LicmRepro2`/`ArrRepro` (all of which read `System.out`, a static reference field, immediately before a string-concat `invokedynamic`) into a crash once `fb4a333d`'s precise-resume path was live. |
| **Discovered** | 2026-07-06/07, verifying the `hib-proxyclassreuse-loader-blind-class-resolution.md` Residual B (`getEnclosingClass`) fix under JIT-on settings, then investigating `fb4a333d`'s Groovy regression per coordinator direction, through several rounds culminating in a full root-cause fix rather than the initial blanket-revert stopgap. |
| **See also** | [hib-proxyclassreuse-loader-blind-class-resolution.md](hib-proxyclassreuse-loader-blind-class-resolution.md) — unrelated, unaffected by this doc's fixes. |

## Summary of the investigation

An earlier pass through this bug (see git history on this doc/branch) bisected
`fb4a333d`'s Groovy regression down to its item 2 (the unconditional
reason-8 routing in `emit_deopt_stubs`) and shipped a blanket revert —
forcing reason 8 back to the pre-`fb4a333d` imprecise "safe reject" — as a
stopgap. **That tradeoff was explicitly rejected**: it closed the Groovy
regression by reopening the exact silent-data-corruption risk `fb4a333d` had
fixed, without ever independently re-confirming that risk with a real repro.
This doc reflects the follow-up investigation that found and fixed the ACTUAL
root causes, so reason 8 could go back to `fb4a333d`'s original, unconditional
precise-resume routing with no tradeoff.

### Step 1 — recovering and running the real repros

`fb4a333d`'s own standalone repros (`AccumRepro`/`AccumRepro2`/`AccumRepro3`,
`MiniRepro`/`MiniRepro2`/`MiniRepro3`, `LicmRepro`/`LicmRepro2`/`LicmRepro3`/
`LicmRepro4`, `ArrRepro`, `LhmRepro`, `CleanRepro`) were never committed to
git; they were recovered from an uncommitted sibling worktree
(`hib-runner/` under a `wt-hib-inpredicate-null-*` worktree) and run 10-15
times each against four binary states (pre-`fb4a333d`, `fb4a333d` as
originally committed, the earlier blanket revert, and each candidate fix),
cross-checked against real HotSpot (`/home/victor/jdk25`) as ground truth.
This immediately surfaced a real, independent bug (see Step 2) that the
earlier bisection pass had missed because it never had the real repros to
run.

### Step 2 — `getstatic` never marked a reference-typed value as an oop (real fix, kept unconditionally)

`../../../jit/src/x64.rs`'s `0xb2` (`getstatic`) codegen has two call sites (the
top-level opcode arm, ~line 19025, and the inlined-callee arm used when a
`getstatic`-containing method is inlined into a caller, ~line 14077). Both
discarded the constant-pool `type_tag` and called `push_from_rax()` (which
always defaults `stack_oop_marks.push(false)`) without ever calling
`mark_top_as_oop()` for a reference-typed (`L`/`[`) static field — unlike
`getfield`'s inline arms, which already carry this exact fix (see their
`c_is_ref`/`type_tag == b'L' || b'['` markers, added for a prior bug,
`jasper-jdt-parser-arrayindexoutofbounds.md`).

`LicmRepro`'s trap shape is `getstatic System.out` (a reference) immediately
followed by a `makeConcatWithConstants` invokedynamic. With the unmarked
slot, the invokedynamic-trap's OSR-exit snapshot recorded `System.out`'s
stack slot as a plain non-oop value; the resumed interpreter frame then
handed `PrintStream.println` a garbage/null receiver, observed as a
`NullPointerException` crash (`LicmRepro`/`LicmRepro2`/`ArrRepro`) once
`fb4a333d`'s precise-resume path was live.

**Fixed** by adding the same `mark_top_as_oop()` call `getfield` already has,
gated on `type_tag == b'L' || type_tag == b'['`, at both `getstatic` codegen
sites. This fix is unconditionally correct and independent of the reason-8
routing question — kept regardless of how the rest of this investigation
resolved.

### Step 3 — the box's `DeoptReason` was misclassified as `OsrExit` instead of `UnreachedCode`

With the getstatic fix alone, restoring reason 8's unconditional precise
routing fixed the repro suite but made Groovy WORSE (0/36, vs the blanket
revert's baseline) — a real, orthogonal problem. `CRATONVM_DBG_DEOPT` traces
showed ~2000 successful frame reconstructions for a single `simpleBean()`
run, all mislabeled `reason=OsrExit`.

Root cause: `emit_osr_exit_map_at` is the SHARED snapshot-recording function
for two call sites — the true loop-header OSR-exit trigger, and the `0xba`
invokedynamic-trap snapshot — and both used to hard-code the recorded box's
`DeoptReason` as `OsrExit`. `real_frame_deopt_resume_and_despeculate`'s
de-speculation step recovers the reason FROM THE BOX
(`compiled.deopt_points.find(|dp| dp.bci == rframe.bci).map(|dp| dp.reason)`),
not from the raw `8` baked into `deopt_stubs` — so an invokedynamic trap that
actually fired was de-speculated using `OsrExit`'s count-based
recompile-and-retry policy instead of `UnreachedCode`'s "give up
immediately" (`MakeNotCompilable`). Since Groovy's `IndyInterface`-based
dynamic dispatch reaches this "uncommon" trap on essentially every call
(never actually unreached), the method never got blacklisted and kept
re-entering the trap on every subsequent invocation.

**Fixed** by giving `emit_osr_exit_map_at` a `_reason` variant
(`emit_osr_exit_map_at_reason`) so each call site stamps its own correct
reason: `UnreachedCode` for the `0xba` trap, `OsrExit` for the loop-header
case.

### Step 4 — three separate call sites never drove de-speculation at all

Fixing Step 3 alone was not enough (still ~1200 mislabeled-correctly-but-
unblacklisted hits). Tracing every consumer of the jit crate's `LAST_DEOPT`
thread-local (populated by `x64_deopt_entry`, drained by `take_last_deopt()`)
found THREE VM-side call sites that each independently mishandled a reason-8
resume:

1. **The legacy first-call JIT tier-up path**, inside `execute()` itself
   (predating `execute_jit_call`/`execute_invokevirtual_cached`) — never
   called `take_last_deopt()` at all, leaking a stashed frame that a LATER,
   unrelated deopt check on the same thread could pick up and misinterpret
   (wrong locals/stack for a completely different bci/method).
2. **`execute_jit_call_decoded`** — called `take_last_deopt()` (correctly, no
   leak) but on a reject just returned `Ok(None)` (re-run interpreted)
   without ever calling `DeoptimizationController::deoptimize`, so
   `UnreachedCode` never got blacklisted through this path.
3. **`try_osr()`'s safe-reject arm** — same gap as #2: consumed the frame,
   rejected the OSR-exit transfer, returned `None` with no de-speculation.

Each of these three is a distinct, real bug: unlike the safe-reject helper
`jit_uncommon_trap` (used when `emit_deopt_stubs` routes reason 8 through
`None`), which synchronously calls `DeoptimizationController::deoptimize` and
so blacklists `UnreachedCode` on first occurrence regardless of caller, the
precise-resume path (`x64_deopt_entry`) relies entirely on ITS caller to
drive de-speculation — and three of the four VM-side callers never did.
Since Groovy's `doCall` methods are invoked mostly through the cached
dispatch paths (#2 above dominates in practice: ~1200 of ~2000 total hits in
one `simpleBean()` run before this fix), fixing only the legacy tier-up path
(#1) was insufficient on its own (Groovy went from 0/36 to 5/36); fixing all
three brought Groovy back to full parity with the pre-existing baseline.

**Fixed** by wiring the same `DeoptimizationController::deoptimize` call
`jit_uncommon_trap` makes into all three sites, using each site's own
already-in-scope method identity (`cached.class_name`/`method_name`/
`method_descriptor`, or the tier-up site's `class_name_str`/`method_name`/
`method_descriptor` lexical bindings), recovering the reason from the
matching `deopt_points` entry (falling back to `UnreachedCode`, the only
reason this snapshot machinery unconditionally records without the
`CRATONVM_DEOPT_REAL` experimental gate).

## Verification

### Corruption-repro suite (15 runs each, HotSpot ground truth in parentheses)

| Repro | Pre-`fb4a333d` | `fb4a333d` as committed | This fix |
|---|---|---|---|
| `AccumRepro` (703) | STABLE, correct | STABLE, correct | STABLE, correct |
| `AccumRepro2` (703) | STABLE, correct | STABLE, correct | STABLE, correct |
| `AccumRepro3` (703) | **NONDETERMINISTIC** (73/5/1405) | STABLE but wrong (1/1) | STABLE but wrong (1/1) — pre-existing, see "Residual" below |
| `LicmRepro` (100000) | STABLE but wrong (2703) | **CRASH** (NPE, 15/15) | **STABLE, correct (100000)** |
| `LicmRepro2` (100000) | STABLE but wrong (2703) | **CRASH** (15/15) | **STABLE, correct (100000)** |
| `LicmRepro3` | STABLE | **CRASH** (15/15) | STABLE |
| `LicmRepro4` (100000) | wrong (2703) | wrong (2703) | wrong (2703) — pre-existing, see "Residual" below |
| `ArrRepro` (100000) | STABLE, correct | **CRASH** (15/15) | **STABLE, correct** |
| `MiniRepro`/`MiniRepro2` | STABLE | STABLE | STABLE |
| `MiniRepro3` | CRASH (AIOOBE — missing CLI arg, matches HotSpot exactly) | same | same |
| `LhmRepro`/`CleanRepro` | STABLE | STABLE | STABLE |

This fix is a strict improvement over BOTH baselines: it fixes every case
`fb4a333d`-as-committed crashed on (`LicmRepro`/`LicmRepro2`/`LicmRepro3`/
`ArrRepro`), matches HotSpot exactly where it now differs from both prior
baselines, and does not regress anything that was already correct.

### `GroovyBeanDefinitionReaderTests`

- **In isolation** (one process per test method, removes cross-test state
  interference): **30/36 pass**, identical on this fix and on the
  already-merged blanket-revert baseline. The 6 failures
  (`contextComponentScanSpringTag`, `springAopSupport`, `springNamespaceBean`,
  `springScopedProxyBean`, `useSpringNamespaceAsMethod`,
  `useTwoSpringNamespaces`) are a pre-existing, unrelated Spring-namespace-URI
  resolution issue (`Namespace prefix: aop is not bound to a URI`),
  reproducing identically under `--nojit` — confirmed out of scope for this
  investigation.
- **Batch** (all 36 methods in one JVM process, via the `KRun` harness):
  **5/36 pass**, again identical on this fix and on the already-merged
  baseline — batch-mode cross-test contamination is pre-existing and
  independent of this fix (confirmed by running the SAME already-merged
  binary in the SAME batch mode).
- `simpleBean()` specifically (the isolated repro used throughout bisection):
  FAIL (`fb4a333d` as committed) → PASS (this fix, and the earlier blanket
  revert).

This fully closes the loop the earlier blanket-revert stopgap could not:
Groovy is not regressed (exactly matches the merged baseline in both
measurement modes), while the corruption-repro suite is fixed without
reopening any tradeoff.

### JIT unit tests

`cargo test -p cratonvm-jit --lib`: 878 passed, 4 failed — the same
pre-existing `aarch64` branch-range-overflow failures present before this
investigation (irrelevant to the x86-64 backend this fix touches). No new
failures.

### `084c8ffb` regression check

`084c8ffb` ("Fix OSR uncommon-trap fallthrough misreading i64::MIN deopt
sentinel as a return value", predates `fb4a333d`) added a `deopt_signaled`
check in `try_osr()`'s fallthrough. That check is untouched by this fix (it
sits immediately after the new de-speculation call added in Step 4, item 3)
and is exercised extensively by `LicmRepro`/`ArrRepro` (OSR + live
invokedynamic), which now pass correctly — confirming no regression.

## Residual (out of scope for this fix, pre-existing)

`AccumRepro3` (wrong count, stable) and `LicmRepro4` (wrong total, stable)
remain incorrect under this fix. Both are wrong in exactly the same way on
the PRE-`fb4a333d` baseline too (`AccumRepro3` nondeterministically; `LicmRepro4`
deterministically at `total=2703` instead of `100000`) — this is a distinct,
pre-existing bug in the safe-reject "continue interpreting" fallback path
itself (i.e., what happens when a genuinely `Unsupported` slot forces a
whole-method re-run), not something `fb4a333d`, the earlier blanket revert,
or this fix's changes introduced, worsened, or are positioned to fix. Left
for separate investigation.

## UPDATE 2026-07-07 (concurrent branch `fix/hib-temporal-placeholder-dup-20260707`) — a FOURTH bug in the same machinery: stashed frames carried no method identity

Merged the same day from the branch that root-caused the Hibernate
`type.temporal.*` `values (??,??)` SQL-placeholder duplication (the visible
Hibernate face of the corruption the earlier blanket revert had reopened —
see `hibernate/hib-temporal-sql-parameter-placeholder-duplication-FIXED.md`).
That investigation independently found one more real unsoundness this doc's
three fixes do not cover:

**The stashed `ReconstructedFrame` carried NO method identity** (`method_key`
was `String::new()` in the x64 producer). When a reason-8 trap fires in a
NESTED compiled callee, its sentinel bubbles up through the compiled callers'
epilogue bails, and the outermost interpreter sink consumed the stash as if it
belonged to the OUTERMOST method — materializing that method's frame with the
callee's locals/stack/bci: arbitrary misexecution. Deterministic repro:
`scratch-min/IndyReplay.java`'s nested shape (compiled middle → compiled leaf
with a side effect before a live indy) corrupted 30000/30000 calls before
these fixes, 0 after. Closed by:

1. Baking `"<class>.<method>:<descriptor>"` into every deopt snapshot
   (`build_and_record_deopt_point`; the OSR and eager first-call compile paths
   now pass real method keys — they passed `""`).
2. Identity checks at every resume consumer (`real_frame_deopt_resume_and_
   despeculate`, `build_deopt_frame_inner`, `try_osr`'s transfer arm); a
   mismatched frame de-speculates its REAL owner (parsed from the key) and
   takes the safe re-run. Unit-tested (`deopt_frame_identity_matching`,
   `mismatched_frame_refused_and_owner_despeculated`).
3. `try_resume_trapped_callee` (vm/src/jit/helpers.rs) + `execute_prebuilt_
   frame`: dispatch helpers resolve a trapped compiled callee PRECISELY at the
   call site (rebuild its frame from the stash, interpret to completion, hand
   the real result to the compiled caller) — nested chains never propagate a
   sentinel or replay side effects at all.
4. `CompiledMethod.has_indy_trap` publication gates (JIT→JIT direct-call
   baking, MIC/PIC inline-cache installs), so machine code never calls an
   indy-trap artifact directly — every call stays on a dispatch helper that
   can resolve its trap. Indy-bearing methods with a non-tail raw
   self-recursive call bail compilation (stash identity cannot distinguish
   recursive invocations).
5. `x64_deopt_entry`'s superseded-epoch short-circuit and the sink's
   `compilation_epoch` freshness check now apply only under
   `CRATONVM_JIT_FREE_CODE` — in the default retain-everything mode the deopt
   boxes are leaked and a stale artifact's snapshot stays self-consistent
   with its own still-executing code, so refusing forced the imprecise re-run
   for every post-despeculation trap arriving via stale cached entries.

The reason-tagging fix in this doc (`emit_osr_exit_map_at_reason`) and that
branch's identical independent fix were unified in the merge; ditto the
sink-side de-speculation (kept as the fallback arm where precise resume is
unavailable). See the placeholder-duplication doc for full verification
numbers (Hibernate temporal: 36 corrupted SessionFactories → 0; nested repro
30000/30000 → 0; 24-class passed-slice regression 24/24).

**Residual flagged for the Azure host:** re-measure
`GroovyBeanDefinitionReaderTests` batch/isolated numbers on the merged tree
(structurally the identity checks can only remove wrong-frame resumes, never
add them; no Spring checkout exists on the Windows box these fixes were
verified on).
