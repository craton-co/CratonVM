# Hibernate `type.temporal.*` — JDBC parameter placeholders duplicate in generated SQL (`values (??,???)`)

**Status:** ✅ **FIXED 2026-07-07** (branch `fix/hib-temporal-placeholder-dup-20260707`) — root-caused to the
reopened JIT invokedynamic uncommon-trap imprecise-resume corruption (the `5ceb880f` revert of `fb4a333d`),
NOT a string-builder defect. Fixed by making the reason-8 precise resume identity-sound and re-enabling it —
closing this bug AND the Groovy regression that forced the revert, at once. See "Root cause" and "Fix" below.
**Severity:** Medium (was: mass test failures in the affected classes; no crash/abort).
**Mode:** default (JIT) real-JDK mode; `--nojit` never reproduced it (the mechanism is JIT-deopt-specific).

## Context

Found while validating the (now archived) GC stale-local crash doc
[`hib-temporal-gc-lambda-native-stale-local.md`](hib-temporal-gc-lambda-native-stale-local.md):
after the 2026-07-06 GC fixes (`3240cb75`), the 5 `org.hibernate.orm.test.type.temporal.*`
classes no longer crash (0 stale-pointer / SIGSEGV / `Object.<sam>` markers) — they run to
completion but fail en masse on a **different, functional** bug.

## Symptom

`LocalDateTimeTest` (all 5 classes affected): many of the 162 tests fail with

```
org.hibernate.exception.SQLGrammarException: Could not prepare statement
[Syntax error in SQL statement "insert into entity_tbl (value_col,id_col) values (?[*]?,??)" ...]
```

(`[*]` is H2's error-position marker.) The INSERT's JDBC `?` placeholders are **duplicated**
(`values (??,??)`, `values (???,??)`, `values (??,???)` — counts vary run to run). Baseline
measured on Windows local, dev `86f37f84`, fresh build: `found=162 ok=54 failed=36 aborted=72`,
all 36 failures this signature (36 corrupted INSERTs; distribution `23× (??,??), 10× (???,???),
2× (???,??), 1× (??,?)`). The original report hypothesized a `StringBuilder` buffer-reset defect
from the "growth" pattern; the run-to-run variation (3,2 → 2,2) refutes that — the counts track
**how much JIT-committed work got re-executed**, not any monotonic state.

## Root cause — NOT a string builder; it is the reopened reason-8 imprecise deopt resume

Hibernate renders the INSERT through `AbstractSqlAstTranslator.renderInsertInto` →
`visitParameter` → `appendSql("?")` into the translator's `sqlBuffer`. Methods on this path are
JIT-compiled after ~50 SessionFactory builds (each test builds a fresh SF). Any compiled method
containing a live `invokedynamic` (lambda creation, string concat — pervasive in this code)
compiles the `0xba` site as an **unconditional uncommon trap** (reason 8, `UnreachedCode`): the
instruction is never JIT-executed; reaching it deopts to the interpreter.

At the time this bug was filed, dev carried `5ceb880f`, which had reverted `fb4a333d`'s precise
reason-8 resume (because it regressed Groovy — see
[`jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression.md`](../jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md)).
The reverted state resolves the trap **imprecisely**: re-run the whole method from entry (normal
call) or continue the live frame from its pre-OSR pc (OSR) — **re-executing every side effect the
compiled code had already committed before the trap**, i.e. appending the `?` (and whatever else
preceded the trap in the re-run scope) a second time into the SAME `sqlBuffer`. The corrupted SQL
is then cached in the SF's mutation plan, failing every statement of that SF.

Two pre-existing defects made this both **worse** and **persistent**:

1. **No frame identity on deopt snapshots.** `fb4a333d`'s precise resume stashed a
   `ReconstructedFrame` with `method_key: String::new()`. When the trap fired in a NESTED
   compiled callee (sentinel `i64::MIN` bubbling up through compiled callers' epilogue bails),
   the outermost interpreter sink resumed the OUTER method's frame with the INNER method's
   locals/stack/bci — arbitrary misexecution. THAT was the actual Groovy "duplicate main method"
   regression (never diagnosed at the time), which forced the revert — reopening this bug.
2. **Wrong-method blacklisting.** `jit_uncommon_trap` attributes the deopt to
   `thread.frames.last()` — the last *interpreter* frame, which for a compiled callee is some
   outer method entirely. The truly-trapping method was never made not-compilable, stayed
   compiled, and re-corrupted every SessionFactory for the rest of the run (36/54 SFs here).

Minimal repro (now at `scratch-min/IndyReplay.java`): a compiled leaf with
`sb.append('?'); return "m" + pos;` called through a compiled middle method. On the pre-fix
binary the nested shape corrupts **30000/30000** calls (`sb == "#??"` — the leaf's append
re-executed by the imprecise re-run); with the fix, 0/30000 on all three shapes
(direct / nested / OSR-loop).

## Fix (this branch) — identity-sound precise resume for reason 8, closing both bugs

1. **Bake method identity into every deopt snapshot** (`jit/src/x64.rs
   build_and_record_deopt_point`): `frame_state.method_key = "<class>.<method>:<descriptor>"`.
   The OSR and eager first-call compile paths now pass their real keys too (they passed `""`).
2. **Identity-check every resume consumer** (`../../../../vm/src/runtime/interpreter.rs`):
   `real_frame_deopt_resume_and_despeculate`, `build_deopt_frame_inner`, and `try_osr`'s
   OSR-exit transfer all verify the stashed frame's `method_key` names the method they are about
   to resume; a mismatch de-speculates the frame's REAL owner (parsed from the key) and falls
   back to the conservative re-run. This is what makes re-enabling reason-8 precise routing safe
   where `fb4a333d` was not.
3. **Precise in-place resolution at dispatch helpers** (`vm/src/jit/helpers.rs
   try_resume_trapped_callee` + `execute_prebuilt_frame`): when a compiled callee invoked by a
   dispatch helper returns the sentinel with a stashed frame whose identity matches that callee,
   the helper rebuilds the callee's interpreter frame at the trapping bci, runs it to completion,
   and hands the REAL result back to the compiled caller. No side effect re-runs; no sentinel
   escapes; nested chains are resolved at the innermost point where the callee's continuation is
   still intact.
4. **Publication gates** so machine code never calls an indy-trap-bearing artifact directly
   (where no helper could resolve the trap): `CompiledMethod.has_indy_trap` (set when the
   compile emitted any `0xba` trap) is consulted by the JIT→JIT direct-call baking
   (`callee_compiler`, the OSR eager direct-call pre-pass) and the MIC/PIC inline-cache installs.
   Such methods always dispatch through a helper. A method containing BOTH an indy and a
   non-tail raw self-recursive call bails compilation entirely (invocation-identity for the
   stash would be ambiguous); tail-jump self-calls remain compiled (same physical frame).
5. **Reason-8 precise routing restored** (`jit/src/x64.rs emit_deopt_stubs`). NOTE: while this
   branch was in flight, a concurrent session independently landed `4224c705` on dev ("Fix
   invokedynamic uncommon-trap deopt: getstatic oop-mark, reason misclassification, missing
   de-speculation") — restoring `fb4a333d`'s unconditional routing, fixing the missing
   `getstatic` reference oop-mark (a snapshot-correctness bug this branch did not find), the
   same OsrExit→UnreachedCode reason mis-tagging (both branches independently added the same
   reason-stamping variant; unified in the merge as `emit_osr_exit_map_at_reason`), and
   sink-side de-speculation (kept in the merge as the fallback arm where precise resume is
   unavailable). The merged tree keeps dev's unconditional routing. That commit did NOT address
   the frame-identity hazard — items 1–4 here remain the closing pieces for nested chains.
6. The instance-method tier-up sink (`execute_jit_call_decoded`) gains the same precise-resume
   arm `execute_jit_call` already had, so interpreter-invoked virtual calls resolve precisely
   too (with `4224c705`'s de-speculation retained as the no-resume fallback).

Every resume path de-speculates the trapping method with its CORRECT identity
(`DeoptimizationController::deoptimize`), so it is evicted/blacklisted and reverts to the
interpreter — fixing defect (2) for this trap family (the `frames.last()` misattribution inside
`jit_uncommon_trap` itself remains for other reason codes' no-snapshot fallback — pre-existing,
lower-stakes, flagged as follow-up).

## Groovy regression status

The Groovy failure mode (cross-method frame resume) is structurally impossible with the identity
checks — a mismatched frame now takes exactly the pre-`fb4a333d` re-run path that was
Groovy-green, and a matched single-frame resume is correct by construction. Empirical
re-verification of `GroovyBeanDefinitionReaderTests` needs the Azure host (no Spring checkout on
this Windows box) and is flagged as follow-up in
[`jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression.md`](../jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md),
whose "a real fix that closes both bugs at once is still needed" follow-up THIS branch implements.

## Verification (Windows local, real JDK 25, JIT on, fresh worktree build)

- `scratch-min/IndyReplay.java` (three trap shapes — direct, nested compiled-middle→compiled-leaf,
  OSR loop — each with a caller-visible side effect committed before a live indy): pre-fix the
  nested shape corrupts **30000/30000** calls (`sb == "#??"`); post-fix **0/30000 on all three
  shapes, 3/3 repeat runs**. `CRATONVM_DBG_DEOPT=1` shows every trap resolving via
  `helper precise-resume of trapped callee` with `action=MakeNotCompilable`.
- `scratch-min/TzDstProbe.java`: expectations validated on HotSpot JDK 25 first, then `@@PASS`
  on CratonVM (winter/summer offsets, exact 2018 EU + NZ transition instants, Calendar
  round-trip, untouched-zone guards).
- `LocalDateTimeTest`: baseline `found=162 ok=54 failed=36 aborted=72` (all 36 = this doc's
  placeholder corruption; `23× (??,??), 10× (???,???), 2× (???,??), 1× (??,?)`) →
  **`ok=88 failed=2 aborted=72`, 0 corrupted statements** (36 placeholder failures gone, 6
  unmasked DST-skew failures fixed by the SimpleTimeZone change). The 2 residual failures are a
  pre-existing empty-message `IllegalThreadStateException` family (also present on the baseline
  binary underneath the corruption; separate, untriaged — not this bug's domain).
- `InstantTests`: baseline `found=204 ok=54 failed=75` (36 placeholder + 37 `typeNamePattern`
  NPEs + 2 ITSE) → **`ok=90 failed=39`** — every placeholder failure gone; the 37
  `NullPointerException: ... "typeNamePattern" is null` (Hibernate `DdlTypeImpl.getRawTypeName`
  called with a null DDL pattern, TIMESTAMP_UTC-remapping envs) reproduce with the IDENTICAL
  count on the pre-fix baseline binary — a separate pre-existing bug, independent of this fix.
- Hibernate passed-category regression slice (`run-hib.sh --category passed --start 100
  --count 24`): **24/24 PASS**.
- `ConcatProbe` (hot `"?"+int` indy concat, 40000 iterations): 0 bad.
- `cargo test -p cratonvm-jit --lib`: 878 passed + the same 4 pre-existing aarch64
  branch-range failures every prior commit in this saga documented. `cargo test -p
  cratonvm-vm --lib deopt`: 52/52 (including the two new identity-gate tests).
- `cargo test -p cratonvm-native-builtins --lib`: now COMPILES again (the cfg fix); 2792 pass,
  13 failures verified pre-existing via A/B against the unmodified base tree (identical set).

Residuals (all pre-existing, none of this bug's family): the `typeNamePattern` NPE cluster and
the `IllegalThreadStateException` pair above, tracked in
[`hib-temporal-residuals-typename-npe-illegalthreadstate.md`](../known-issues/hib-temporal-residuals-typename-npe-illegalthreadstate.md);
Groovy re-measurement on the Azure host (flagged in the jit-invokedynamic FIXED doc).

## Repro (historical)

```bash
cd apps/hib-suite-runner
echo org.hibernate.orm.test.type.temporal.LocalDateTimeTest > /tmp/one.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm> --java-home <jdk25> \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner /tmp/one.txt 0
# grep the log for: values (?? — any doubled placeholder is the bug
```

Standalone: `scratch-min/IndyReplay.java` (fails `nested bad=30000` pre-fix, `@@PASS` post-fix).

## Final verification on the MERGED tree (post `643a0c92`, unified with dev's `4224c705`)

Full 5-class run, merged binary, otherwise-idle Windows host, `--Xmx 1500m`, JIT on —
**zero `Syntax error` / zero duplicated placeholders across all five classes**:

| class | result | residual attribution |
|---|---|---|
| `InstantTests` | `found=204 ok=92 failed=37` | all 37 = pre-existing `typeNamePattern` NPE (identical count on the pre-fix baseline binary) |
| `LocalDateTimeTest` | `found=162 ok=86 failed=4` | ITSE flakiness ±2 + 1 `JUnitException` close-context + 1 assertion — all pre-existing families, 0 placeholder |
| `OffsetDateTimeTest` | `found=252 ok=110 failed=86` | dominated by the same pre-existing `typeNamePattern` NPE family (first full measurement on this class); dev's own `9ce1d1ba` note records its 2 gracefully-degraded `mark_young implausible extent` GC rejections as the open gcstress residual |
| `OffsetTimeTest` | `found=396 ok=74 failed=230` | same `typeNamePattern` family (351 occurrences pooled across the Offset classes) |
| `ZonedDateTimeTest` | `loaderror=OutOfMemoryError` after ~29 min | the OPEN GC implausible-header livelock dev's `9ce1d1ba` doc records for exactly this class ("LIVELOCKS in a continuous mark_young rejection loop") — on Windows the livelock fills the 1500m heap → OOM; reproduces identically on the pre-fix baseline binary |

Unit gates on the merged tree: `cargo test -p cratonvm-jit --lib` = 878 passed + the same 4
pre-existing aarch64 failures; `cargo test -p cratonvm-vm --lib deopt` = 52/52;
`cargo test -p cratonvm-native-builtins --lib` = 2793 passed / 12 failed (all 12 verified
pre-existing by A/B against the unmodified base tree; dev independently fixed a 13th).
`IndyReplay` (3 shapes) + `TzDstProbe`: `@@PASS` on the merged binary.

The pre-existing residual families are archived in
[`hib-temporal-residuals-typename-npe-illegalthreadstate.md`](../known-issues/hib-temporal-residuals-typename-npe-illegalthreadstate.md)
(typeNamePattern NPE + ITSE) and
[`../gcstress-residual-corruption-faces-FIXED.md`](../gcstress-residual-corruption-faces-FIXED.md)
(the now-fixed ZonedDateTimeTest GC livelock face).

## Concurrent finding merged 2026-07-07 — two more affected classes (independent Windows rerun)

While this fix was in flight, a concurrent session's 4-shard Windows rerun of the 121-class
non-passed list (dev `d0a779f6`) independently reproduced the identical growing-duplicate-`?`
signature on two classes OUTSIDE `type.temporal.*`:

- `org.hibernate.orm.test.bytecode.enhancement.basic.ExtendedEnhancementNonStandardAccessTest`
  (`insert into abstract (...9 columns...) values (????,????,????,…)`)
- `org.hibernate.orm.test.query.hql.FunctionTests`
  (`insert into EntityOfLists_basic (…) values (????,????,????)` during fixture setup)

That observation matches this doc's root cause exactly (the corruption is a general
JIT-deopt side-effect replay on ANY hot SQL-string-building path, nothing temporal-specific)
and predicted the "widen the search scope" conclusion this fix already embodies. Both classes
are expected fixed by this branch (same mechanism, same paths); re-verify them in the next
non-passed-list rerun rather than as a blocking gate here.

**Harness gotcha recorded by that session (still open, worth fixing separately):**
`rerun.sh`/`rerun-linux.sh` categorize `PASS` as `failed==0 && aborted==0 && found>0` without
requiring `ok == found` — a class that dies in fixture setup before any @Test runs
(`found=123 started=0 ok=0`) is mis-reported as PASS. Any "PASS" row with `ok != found` is a
false positive.
