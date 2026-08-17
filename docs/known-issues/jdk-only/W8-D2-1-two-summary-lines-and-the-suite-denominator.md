# W8-D2-1 — two REGRESSION SUITE lines in one log, and why neither pair has a denominator

Status: **instrument audit — no VM defect found here.** The subject is
`regression-suite/run.sh` and the log `/c/craton/suite-clean.log` (10282 bytes,
mtime 2026-08-13 00:07).

Scope note: this lane could not build or run CratonVM (a release build was in
flight) and did not. Everything below is read off the script, the log's bytes,
and the git index. Where a claim could not be closed from those artifacts it is
labelled UNRESOLVED rather than guessed.

---

## Summary of findings

1. `missing:` is **not** the `STRICT_COVERAGE` gate. It is its dual: a name in
   `CORE_CLASSES`/`JDKONLY_CLASSES` for which `src/<name>.java` does not exist
   **on disk at the moment the script starts**. It says nothing about the VM.
2. There are **not two phases**. `SUITE=all` is one list and one loop, and
   `run.sh` prints its summary exactly once. Two summary lines mean **two
   `run.sh` processes wrote to the same file**, at independent file offsets. The
   log is a byte-level mishmash of both, and most of the second run's output was
   overwritten by the first run's.
3. **Yes, both lines double-count**, in two independent ways, and the second one
   double-counts nine times. `21 failed` is not 21 vectors in either line.
4. The exit status is *arithmetically* right (`exit 1` iff `total_fail != 0`),
   and a `missing:` alone, or a `harness:` alone, **does** fail the build. But
   "the run exited 1" cannot be attributed to either summary from this log.

**Neither printed pair may be quoted as-is.** What may be quoted, with caveats,
is in [§6](#6-what-may-be-quoted).

---

## 1. What `missing:` means

`run.sh` computes it *before anything runs*, at lines 188-198:

```sh
MISSING_CLASSES=""
prune_missing() {
  PRUNED=""
  for c in $1; do
    if [ -f "$HERE/src/$c.java" ]; then PRUNED="$PRUNED $c"
    else MISSING_CLASSES="$MISSING_CLASSES $c"; fi
  done
  PRUNED="${PRUNED# }"
}
prune_missing "$CORE_CLASSES";    CORE_CLASSES="$PRUNED"
prune_missing "$JDKONLY_CLASSES"; JDKONLY_CLASSES="$PRUNED"
```

and reports it at the end, at lines 581-584:

```sh
for c in $MISSING_CLASSES; do
  echo "  LIST ERROR: '$c' is in a class list but src/$c.java does not exist"
  total_fail=$((total_fail+1)); total_failed="$total_failed missing:$c"
done
```

So the condition is exactly: **`[ -f "$HERE/src/$c.java" ]` was false at script
start.** The vector is then removed from the schedule — it never runs, on either
VM — and one point is added to `total_fail` anyway. That is deliberate and the
rationale is in the file's own comment (lines 165-168): a renamed vector used to
be filtered out silently, which turned "this coverage is gone" into a slightly
smaller pass count that nobody diffs.

The four gates are four different populations and are prefixed differently:

| prefix | condition | fatal? |
|---|---|---|
| `missing:<C>` | `<C>` is in a class list, `src/<C>.java` absent | always |
| `stale:<C>` | `<C>` is in `UNREGISTERED_CLASSES`, `src/<C>.java` absent | always |
| `unregistered:<C>` | `src/<C>.java` exists, `<C>` in no list | only under `STRICT_COVERAGE=1` (else a WARNING) |
| `harness:<C>` | a G1-G4 guard fired for `<C>` | always |

`STRICT_COVERAGE=1` is the `unregistered:` gate (lines 594-605). It is **not**
what produced these 19 lines, and it was not set in this run — no
`COVERAGE WARNING` line appears anywhere in the log.

`harness-uncounted.txt` is the escape hatch for **G3 only** (the "publishes no
check count" half), and it is a two-way ratchet: `harness-guard.sh:147-152`
fires an error at a vector that *starts* publishing a count while still listed.
It has nothing to do with `missing:`.

### Why the four long-standing fixtures were "missing"

They were not missing from this worktree. `src/RJdkFormatLocale.java`,
`RJdkStrictMath.java`, `RJdkByteOrder.java` and `RJdkIntrinsics.java` all exist
here and are committed (`67146db71`, `88eaeacfb`, `d194c3945`, `f0a472dcf`) —
and in the **second** summary all four are absent from the failed list except
`RJdkIntrinsics`, i.e. `RJdkFormatLocale` / `RJdkStrictMath` / `RJdkByteOrder`
ran and PASSED.

The first summary was produced by a **different `run.sh` process whose `$HERE/src`
did not contain them when it started.** The proof is arithmetic and exact:

* `run.sh` at `c59efd3eb` (the revision both runs used — see §2) registers
  `CORE_CLASSES` = 53 and `JDKONLY_CLASSES` = 36, **89** scheduled.
* Run **B**: `77 passed` + 12 non-`harness:` failures = **89**. Nothing missing.
* Run **A**: 89 − 19 `missing:` = **70** scheduled; `69 passed` + 1 failure
  (`RExceptions`, the only non-prefixed entry) = **70**.

Both close exactly. Run A simply had 19 of the 89 `.java` files absent at start.

### The mechanism that can do this inside one worktree

`git checkout` writes the working tree in **index order**, which is
lexicographic. Verified in this repo:

```
63:regression-suite/run.sh
65:regression-suite/src/RArrayStoreTiers.java
76:regression-suite/src/RCollections.java
```

`regression-suite/run.sh` is written **before every `regression-suite/src/*.java`**.
A suite launched during a branch switch or a merge therefore reads the **new**
class lists (`run.sh`, already written) against the **old** source tree
(`src/`, not yet written), and reports every not-yet-written vector as
`missing:`. `prune_missing` runs in the first ~200 lines — milliseconds after
launch — so the window is exactly the width of the checkout.

**UNRESOLVED:** whether run A was this worktree mid-checkout, or a second tree.
A scan of every `regression-suite/run.sh` under `C:/craton/cratonvm/.claude/worktrees/`,
`C:/craton/wt-*`, `C:/craton/cratonvm` and `C:/data/data/cratonvm-worktrees/`
found exactly **one** copy carrying the wave-C registrations (this worktree,
`src` complete). So either A was this tree during a checkout window, or A's tree
has since been updated. **The log cannot answer this, because `run.sh` never
prints which tree, which revision or which PID it is.** That is nomination N1.

---

## 2. Why there are two summaries

`run.sh` emits the summary once, at line 623, and the very next line is the
script's last:

```sh
echo "REGRESSION SUITE: $total_pass passed, $total_fail failed${total_failed:+ ( failed:$total_failed )}"
[ "$total_fail" -eq 0 ]
```

There is no loop around it. `SUITE=all` is a *list selector*, not a two-phase
mode (lines 207-212): it concatenates `$CORE_CLASSES $JDKONLY_CLASSES` into one
`$CLASSES`, consumed by the single `for c in $CLASSES` loop in `run_pass`. The
only other multi-pass mode is `RELEASES=`, which prints `--release N: X passed,
Y failed` per level and still one summary at the end; `RELEASES` was unset here.
`grep` over the whole worktree finds the string `REGRESSION SUITE:` emitted from
`run.sh` and nowhere else.

**Therefore: two summaries = two `run.sh` executions.** They are neither two
arms of a comparison nor a partial phase. Nothing in the suite ever produces
two.

### The two processes shared one output file, at independent offsets

This is provable to the byte:

* The file is 10282 bytes, **0 NUL bytes**.
* Run A's summary line ends at byte offset **5085** (its `\n`). A's summary is
  the last thing A prints, so A's entire output is bytes 0..5085 — 5086 bytes.
* Byte 5086 onward is `" checks=N'.\n  RJdkIntrinsics2 FAIL ..."`.

`" checks=N'."` is the 11-byte **tail** of a G3 line whose full text is
`    Emit 'PASS RJdkIntrinsics (N checks)' or 'CK RJdkIntrinsics checks=N'.`
— 76 bytes. Its first 65 bytes were overwritten by A. And the next line the file
shows is `RJdkIntrinsics2 FAIL`, i.e. **the vector immediately after
`RJdkIntrinsics`** in the schedule. B's stream is continuous across the seam.
Nothing else explains a straddled line: a sequential single writer cannot
produce one, and an appending second writer would show B's own beginning.

Corroboration: B reports `77 passed`, but only **33** PASS lines survive in the
file. 44 of B's PASS lines, and 3 of its 12 FAIL lines, were overwritten by A.

The write pattern is: **B started first**; **A started second and opened the same
path with `>` (O_TRUNC)**; both then wrote at their own `lseek` offsets, so
bytes 0..5085 ended up A's and 5086..10281 ended up B's, with no hole because
A's length met B's offset. `/c/craton/suite-clean.log` is not written by
anything in the repo — no script under `regression-suite/` references it — so it
is a hand-typed redirect target, and two lanes using the same conventional
filename is all it takes.

**Which is authoritative:** run **B** (`77 passed, 21 failed`). Run A's 19
`missing:` entries are facts about A's *filesystem*, not about the VM: those 19
vectors were never executed by A on either VM. A also never ran
`RFsSingleton` / `RJdkOptionalShape` / `RSimpleDateFormatZone` — which confirms
both processes used the `c59efd3eb` `run.sh`, not the working-tree copy edited
at 00:22 that registers those three.

**Contamination check (partial):** both processes `rm -rf "$BUILD"` in
`compile_suite` (line 322) and `rm -rf "$GUARDTMP"` in `run_pass` (line 454),
with `$BUILD` = `$HERE/build` and `$GUARDTMP` = `$HERE/.guard-tmp` holding two
**fixed** filenames, `hs.raw` and `cv.key`. If A and B shared `$HERE` they
clobbered each other's class files and each other's oracle capture. Evidence
*against* that having happened: A started after B (byte-offset argument above)
and still completed 70 vectors with exactly one failure — A's own `rm -rf
"$BUILD"` landing on B mid-loop should have produced a cluster of
`no PASS line` failures in B, and B's failure list contains no such cluster.
That is suggestive, not conclusive. It is the residual reason a solo re-run is
required before quoting anything (§6).

### 2a. Corroboration from a neighbouring log: `run.sh` *is* being rewritten under a running bash

`/c/craton/suite-2313.log` (2026-08-12 23:43, 3426 bytes) is a third suite run,
24 minutes before `suite-clean.log`. It ran cleanly through
`RJdkAsyncChannel` — the second-to-last vector — and then died:

```
  RJdkAsyncChannel PASS
run.sh: line 622: syntax error near unexpected token `('
run.sh: line 622: `  echo "  HARNESS: $total_hbad vector(s) reported on a comparison the suite cannot see:${total_hfaile
```

Two things make this decisive rather than anecdotal:

* **The line number is wrong.** That `echo` is at line **616** in the working
  tree, at **616** in `c59efd3eb`, and at **616** in `fa7d43519`. Bash reported
  **622**. Its line counter had tracked one file layout while the bytes it
  resumed reading came from another — the textbook signature of a script
  rewritten under a running bash, which reads a script lazily and re-reads from
  a saved **byte offset**.
* **The fragment is mid-line.** `near unexpected token '('` on `vector(s)` means
  bash resumed *inside* the line, past the opening `"`, so `(s)` parsed as a
  subshell. A whole-line re-read could not produce that error.

So `run.sh` being edited mid-flight is not a hypothesis in this session — it is
demonstrated, twice: once destroying that run's summary outright, and once (§1)
as the leading explanation for run A's 19 `missing:` entries, since a checkout
or an editor rewrite hits `regression-suite/run.sh` before
`regression-suite/src/*.java`.

**It also independently corroborates run B's numbers, which matters for §6.**
That 23:43 run scheduled the `fa7d43519` lists — `CORE` 46 + `JDKONLY` 35 =
**81** — and produced **77 PASS and 4 FAIL** before dying at the summary.
`c59efd3eb` adds exactly the 8 wave-C fixtures, all expected red:
81 + 8 = **89**, and 4 + 8 = **12**. Run B reported **77 passed, 12 failed of
89**. The pass count is identical and the failure count moves by exactly the 8
new registrations. Two runs, separated in time, agree.

---

## 3. Do the two lines double-count? Yes — twice over

`total_pass` counts one population only: **scheduled vectors that passed**
(`run_pass`, line 516). `total_fail` is an **exit-status accumulator** that sums
four incommensurable populations:

```sh
# run_pass, line 517           scheduled vector failed
else fail=$((fail+1)); failed="$failed $c"; ...

# line 583                     listed, no source — NEVER SCHEDULED
total_fail=$((total_fail+1)); total_failed="$total_failed missing:$c"

# line 587                     stale UNREGISTERED entry — NEVER SCHEDULED
total_fail=$((total_fail+1)); total_failed="$total_failed stale:$c"

# line 598                     source, no registration — COMPILED, NEVER RAN
total_fail=$((total_fail+1)); total_failed="$total_failed unregistered:$c"

# line 619                     a per-vector INSTRUMENT FLAG
total_fail=$((total_fail+total_hbad))
total_failed="$total_failed$(printf '%s' "$total_hfailed" | sed 's/ / harness:/g')"
```

So, explicitly:

**Second line — `77 passed, 21 failed`.** 21 = **12 scheduled vectors** +
**9 harness flags**. All nine harness names —
`RExceptions RArrayStoreTiers RArrayStoreInterfaces RJdkIntrinsics
RJdkIntrinsics2 RShutdownHooks RSimpleTimeZoneRaw RJdkStringCodePoints
RJdkMapViews` — are a **strict subset of the 12**. Every one is counted twice
and named twice. `77 + 21 = 98` against **89** scheduled. The honest reading is
**89 scheduled, 77 passed, 12 failed**, of which 9 also tripped a harness guard.

**First line — `69 passed, 21 failed`.** 21 = **1 scheduled vector**
(`RExceptions`) + **19 `missing:`** (never scheduled) + **1 harness flag** (on
that same `RExceptions`). `69 + 21 = 90` against **70** scheduled. The honest
reading is **70 scheduled, 69 passed, 1 failed**, plus 19 unrunnable
registrations.

So the two "21"s are numerically equal and semantically unrelated. Neither pair
shares a denominator.

### The nine harness flags are not nine findings

This is the part worth acting on. `run_pass` calls the extract guard
unconditionally, regardless of the vector's own verdict (lines 513-514):

```sh
printf '%s\n' "$cvkey" > "$GUARDTMP/cv.key"
harness_guard_extract "$c" "$GUARDTMP/cv.key" || guarded=1
```

and `harness_guard_extract` fires G2 when nothing survived the filter
(`harness-guard.sh:118-121`):

```sh
if [ "$gc_ck" -eq 0 ] && [ -z "$gc_count" ]; then
  if [ "$gc_lines" -eq 0 ]; then
    ... "[G2] $gc_class: nothing survives extract() — the cross-VM diff compares two empty strings"
```

A vector that throws `AssertionError` before printing its banner leaves `cv.key`
**empty by construction**. G2 then fires, and G3 fires with it (no check count
can be parsed out of an empty file). So *every* hard-failing vector that is not
in `harness-uncounted.txt` automatically produces a harness flag as an
arithmetic consequence of having failed.

That is exactly the observed pattern: 9 of the 12 failures carry G2+G3, and the
3 that do not — `RImmutableFactoryTypes`, `RJdkProxyIface`, `RJdkForeign` —
are the three that got far enough to emit `CK` lines before dying. **Zero of
the nine is an independent finding about the instrument.**

The guards G2/G3 were built to catch a *different* shape — a vector that
**PASSES** with nothing observable, the `RDataInputFastPull` defect recorded in
`W7-60-harness-extract-blindness.md`. On a red vector they restate the red.

The oracle-side guards are different and must keep counting: `[G4]
RSimpleTimeZoneRaw` and `[G4] RJdkStringCodePoints` ("the HotSpot oracle exited
0 but printed no PASS line") and `[G1] RShutdownHooks` ("the oracle printed 1
line(s) that extract() DELETES") say the **ground truth is bad**, which is news
whatever CratonVM did — and per this project's own rule, HotSpot is the oracle.
Those three deserve their own investigation; two newly-registered wave-C vectors
are red on HotSpot 25 as configured.

---

## 4. Is the exit status right?

The script ends (lines 623-624):

```sh
echo "REGRESSION SUITE: ..."
[ "$total_fail" -eq 0 ]
```

`set +e` is in force and there is no trailing `exit`, so the script's status is
that `test`'s: **0 iff `total_fail` is 0, else 1**. Given §3, that means:

* a single `missing:` entry, with every scheduled vector green, **exits 1**;
* a single `harness:` flag, with every scheduled vector green, **exits 1**;
* a single `unregistered:` under `STRICT_COVERAGE=1`, likewise **exits 1**;
* `run_pass` returning non-zero (a `javac` failure) short-circuits to
  `exit 3` (line 537), and `SUITE=<garbage>`, an empty schedule or a missing
  binary also `exit 3`.

So the gate cannot silently pass, which is the property that matters most, and
`missing:`/`harness:` failing CI **is the design** — both are documented in the
script as deliberate. No defect in the exit logic.

The defect is upstream of it: **the exit status is a single bit over four
populations**, so "the suite exited 1" carries no information about *which*
gate fired, and the summary line it is printed next to cannot be decomposed by
anyone who has not read the script. That is precisely how this audit was needed.

And on this specific run: **"the run exited 1" cannot be attributed.** Two
processes ran; the shell reported the status of whichever one the caller waited
on. Both would have exited 1 for different reasons. Do not read this pipeline's
exit as the subject's.

---

## 5. Was anything here a VM finding?

Only one entry in the first summary is a scheduled-vector failure at all:
`RExceptions`, which is red in **both** runs and predates the wave-C
registrations. Everything else in the first line is `missing:` — the instrument
describing its own filesystem.

In the second summary, of the 12 scheduled failures, **8 are the newly
registered wave-C fixtures** (`RArrayStoreTiers RArrayStoreInterfaces
RJdkIntrinsics2 RShutdownHooks RSimpleTimeZoneRaw RImmutableFactoryTypes
RJdkStringCodePoints RJdkMapViews`), expected red against a binary that predates
their fixes. The remaining 4 are `RExceptions`, `RJdkIntrinsics`,
`RJdkProxyIface`, `RJdkForeign`. Three of the 8 (`RShutdownHooks`,
`RSimpleTimeZoneRaw`, `RJdkStringCodePoints`) additionally have a **broken
oracle** (G1/G4) and so cannot be scored against HotSpot at all until their
`.java` is fixed — their redness is currently uninterpretable in either
direction.

---

## 6. What may be quoted

**Neither printed line, verbatim.** Both are `X passed, Y failed` pairs with no
shared denominator (§3).

**Do not quote the first line at all.** `69 passed, 21 failed` describes a tree
that was missing 19 of the 89 registered vectors; 19 of its 21 "failures" are
vectors that never executed, and its 69 is out of 70, not 89. It is a fact about
a filesystem.

**The second line may be restated, not quoted**, as:

> `run.sh` @ `c59efd3eb`, `SUITE=all`, `CRATONVM_ARGS=--jdk-only`, `TIMEOUT=420`:
> **89 vectors scheduled, 77 passed, 12 failed.** Of the 12, 8 are the wave-C
> fixtures registered against a binary that predates their fixes and are
> expected red; 3 of those 8 also have a red/silent HotSpot oracle (G1/G4) and
> are therefore not scorable yet. 9 of the 12 additionally tripped the G2/G3
> extract guards, which is the automatic consequence of failing before the
> banner and is **not** 9 further failures.

with these caveats attached, all of them load-bearing:

1. **A second `run.sh` process was running concurrently against the same log
   path.** Whether it shared `$HERE` — and therefore `build/` and
   `.guard-tmp/hs.raw` — is UNRESOLVED (§2). This caveat is **substantially
   relieved** by §2a: an independent run 24 minutes earlier, on the previous
   revision's 81-vector list, produced the same **77** passes with **4**
   failures, and `c59efd3eb` adds exactly the 8 expected-red wave-C fixtures
   (81+8 = 89, 4+8 = 12). Contamination severe enough to matter would have to
   have reproduced that arithmetic exactly. Treat 77/12 as corroborated but not
   solo-confirmed.
2. Most of run B's own output was destroyed by the overwrite; 44 of its 77
   PASSes and 3 of its 12 FAILs are not in the log and are inferred from its own
   summary line, not observed.
3. The run used the **committed** `c59efd3eb` `run.sh` (89 registrations), not
   the working-tree copy edited at 00:22 (92 registrations). A re-run with the
   current file schedules `RFsSingleton`, `RJdkOptionalShape` and
   `RSimpleDateFormatZone` as well, and the denominator becomes 92.

**Recommended before quoting anything:** one solo `SUITE=all` run, to a
PID-unique log path, in a tree with no checkout in flight, with N1 applied so
the banner proves it was solo.

---

## 7. Nominations for `regression-suite/run.sh`

This lane may not edit `run.sh`. Exact literal patches follow. Each `old` string
was verified unique in the working-tree file.

### N1 — print which run this is (the one that would have prevented this audit)

Without it, a log with two summaries is unresolvable, and a `missing:` line
cannot be attributed to a tree. This is the highest-value change here.

*old:*

```
[ -x "$CV" ] || { echo "ERROR: CratonVM binary not found: $CV (build with build-cpu.bat)"; exit 3; }
```

*new:*

```
# Identify the RUN, not just the result. Two run.sh processes sharing one
# redirect target produce two summaries in one file with no way to tell which
# tree, which revision or which schedule each belongs to — and because
# `git checkout` writes the working tree in index order, where
# regression-suite/run.sh (entry 63) precedes regression-suite/src/*.java
# (65+), a suite launched during a branch switch reads the NEW class lists
# against the OLD sources and reports every not-yet-written vector as
# `missing:`. Both happened on 2026-08-13 and cost a full audit to reconstruct
# from byte offsets. See docs/known-issues/jdk-only/
# W8-D2-1-two-summary-lines-and-the-suite-denominator.md.
echo "== RUN pid=$$ tree=$HERE rev=$(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo '?') suite=${SUITE:-core} scheduled=$(printf '%s' "$CLASSES" | wc -w) missing=$(printf '%s' "$MISSING_CLASSES" | wc -w) =="
[ -x "$CV" ] || { echo "ERROR: CratonVM binary not found: $CV (build with build-cpu.bat)"; exit 3; }
```

### N2 — make the summary state what it counts (the denominator fix)

Two patches. First, snapshot the only population commensurate with
`$total_pass`, before the other three are folded in.

*old:*

```
echo "---------------------------------------------"
```

*new:*

```
# Snapshot the ONLY failure population that shares a denominator with
# $total_pass — scheduled vectors that ran and lost — before list errors,
# coverage errors and harness flags are folded into $total_fail below.
total_vecfail=$total_fail
echo "---------------------------------------------"
```

Second, print the decomposition next to the pair.

*old:*

```
echo "REGRESSION SUITE: $total_pass passed, $total_fail failed${total_failed:+ ( failed:$total_failed )}"
[ "$total_fail" -eq 0 ]
```

*new:*

```
# $total_fail is the EXIT-STATUS accumulator and sums four incommensurable
# populations: scheduled vectors that lost; registrations with no source and
# sources with no registration (neither ever ran); and one point per vector
# whose instrument-blindness guard fired (which is a FLAG ON a vector, usually
# one already counted red above — not another vector). Only the first shares a
# denominator with $total_pass, so "$total_pass passed, $total_fail failed" is
# not a pair and must never be quoted as one. Print the decomposition so the
# reader does not have to read this script to interpret the line.
echo "REGRESSION SUITE: $total_pass passed, $total_fail failed${total_failed:+ ( failed:$total_failed )}"
echo "  COUNTS: $total_pass of $((total_pass+total_vecfail)) SCHEDULED vectors passed; $total_vecfail scheduled vectors failed; $((total_fail-total_vecfail-total_hbad)) list/coverage errors (never scheduled); $total_hbad harness-blindness flags (per-vector flags, not extra vectors)."
[ "$total_fail" -eq 0 ]
```

Known imprecision in this patch, stated rather than hidden: under `RELEASES=`,
line 557 adds one point for a level whose compile failed, and that lands inside
the `total_vecfail` snapshot. With `RELEASES` unset — every invocation discussed
here — the snapshot is exactly the scheduled-vector failures.

### N3 — stop counting G2/G3 against a vector that already failed

Judgement call, and weaker than N1/N2 — apply it only if you agree with the
reasoning in §3. It keeps every message printed and changes only the count. The
oracle-side guards (G1/G4) keep counting unconditionally, because "your ground
truth is bad" is an independent finding whatever CratonVM did.

Three patches.

*old:*

```
    HARNESS_GUARD_MSGS=""; guarded=0
```

*new:*

```
    HARNESS_GUARD_MSGS=""; guarded=0; guarded_oracle=0
```

*old:*

```
      harness_guard_oracle "$c" "$GUARDTMP/hs.raw" "$hsrc" || guarded=1
```

*new:*

```
      harness_guard_oracle "$c" "$GUARDTMP/hs.raw" "$hsrc" || { guarded=1; guarded_oracle=1; }
```

*old:*

```
    if [ "$guarded" -ne 0 ]; then
      hbad=$((hbad+1)); hfailed="$hfailed $c"
      printf '%s\n' "$HARNESS_GUARD_MSGS"
    fi
```

*new:*

```
    if [ "$guarded" -ne 0 ]; then
      printf '%s\n' "$HARNESS_GUARD_MSGS"
      # COUNTED only when it is an INDEPENDENT finding.
      #   G1/G4 (oracle-side) always are: a sick or silent oracle invalidates
      #   the ground truth whatever CratonVM did.
      #   G2/G3 read CratonVM's surviving output, so on a vector that ALREADY
      #   FAILED they merely restate the failure — a vector that throws before
      #   its banner leaves an empty cv.key BY CONSTRUCTION, and G2+G3 then fire
      #   for it every single time. On 2026-08-13 all 9 harness flags in a
      #   SUITE=all run sat on vectors already counted red: 9 duplicate points
      #   in "21 failed", 0 independent findings. The blindness G2/G3 exist to
      #   catch (W7-60) is a vector that PASSES with nothing observable, so ask
      #   them about a PASS. The messages print above either way, so a red
      #   vector that is also uncounted is still visible — it just does not
      #   inflate the number.
      if [ "$state" = PASS ] || [ "$guarded_oracle" -ne 0 ]; then
        hbad=$((hbad+1)); hfailed="$hfailed $c"
      fi
    fi
```

### N4 — make `run.sh` immune to being edited mid-run (optional, and it has a trap)

Lower priority than N1/N2, and it must be applied as **both** patches or not at
all. §2a shows an edit to `run.sh` during a run resumed bash mid-line and killed
a 77-pass run at the summary. Re-executing from a private snapshot closes that.

**The trap:** `ROOT` is derived from `dirname "${BASH_SOURCE[0]}"`, so a naive
snapshot makes `git rev-parse` run in the temp directory, fail, and fall back to
`echo C:/craton/CratonVM` — which on this case-insensitive filesystem **is the
main checkout** (`C:/craton/cratonvm`, currently `dev`). The suite would then
silently measure a different tree. The second patch is what prevents that, and
it is worth applying **even without the first**: that fallback is a live hazard
today for any invocation where `git rev-parse` fails for any reason.

*old:*

```
ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)"
```

*new:*

```
# Resolve the tree from the ORIGINAL script path, never from a snapshot copy's
# location. If this is ever allowed to fall back, note what it falls back TO:
# on a case-insensitive filesystem C:/craton/CratonVM IS C:/craton/cratonvm, the
# main checkout — so a failed `git rev-parse` silently measures a DIFFERENT tree
# instead of erroring. The banner above prints $HERE precisely so that is visible.
_self="${CRATONVM_SUITE_SELF:-${BASH_SOURCE[0]}}"
ROOT="$(git -C "$(dirname "$_self")" rev-parse --show-toplevel 2>/dev/null || echo C:/craton/CratonVM)"
```

*old:*

```
set +e
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
```

*new:*

```
set +e
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1

# Bash reads a script LAZILY, by BYTE OFFSET. Editing run.sh while a run is in
# flight makes the running shell resume mid-line inside the NEW bytes: on
# 2026-08-12 23:43 that killed a 77-pass run outright with
#   run.sh: line 622: syntax error near unexpected token `('
# on an `echo` that sits at line 616 in every revision of this file. Re-exec
# from a private snapshot so the bytes being executed cannot be edited under us.
# $CRATONVM_SUITE_SELF carries the ORIGINAL path forward — ROOT is derived from
# it below, and deriving ROOT from the snapshot's location instead would send
# the whole suite at another tree.
if [ -z "${CRATONVM_SUITE_SELF:-}" ]; then
  _snap="${TMPDIR:-/tmp}/cratonvm-run-$$-$RANDOM.sh"
  cp "${BASH_SOURCE[0]}" "$_snap" || { echo "ERROR: cannot snapshot ${BASH_SOURCE[0]}"; exit 3; }
  export CRATONVM_SUITE_SELF="${BASH_SOURCE[0]}"
  bash "$_snap" "$@"; _rc=$?
  rm -f "$_snap"
  exit "$_rc"
fi
```

---

## 8. Was any of this a misreading?

Partly, and it is worth saying plainly: **`missing:` is working exactly as
designed and documented in the script**, and so is the exit status. The first
summary was not a bug — it was a correct report about a source tree that was
missing 19 registered vectors, printed by a process nobody knew was running.

What is genuinely defective is (a) that `run.sh` prints no run identity, so a
second concurrent process is invisible and its `missing:` list unattributable,
and (b) that `total_fail` sums four populations against a `total_pass` that
counts one, so **neither printed pair is a pair**. Both are instrument defects,
not VM findings, and both are addressed by N1/N2.

## §8 — Run A is identified (orchestrator, after this record was written)

This lane could not name run A's working directory and correctly declined to
guess, offering a `git checkout` index-order race as *a* mechanism. The actual
cause is simpler and is the orchestrator's:

1. A suite run was launched from a SNAPSHOT COPY of `run.sh` placed in
   `/c/craton/`, on the theory that a copy could not be edited mid-run.
2. `run.sh:54` resolves `ROOT` from `BASH_SOURCE` — `/c/craton` is not a git
   repo, so `ROOT` fell back to the hard-coded `C:/craton/CratonVM`, **a
   different tree that exists**. That tree does not carry the wave-C fixtures,
   which is precisely the 19 `missing:` entries. The arithmetic this record
   derives (89 − 19 = 70 scheduled) is that tree's `src/`, not a checkout race.
3. That run was cancelled with `TaskStop` — which stopped the task WRAPPER.
   The `bash` and `java` processes underneath kept running, and kept writing to
   `/c/craton/suite-clean.log`.
4. The replacement run (B) was then started in the real worktree with `>` onto
   **the same log path**, so two live processes shared one target at
   independent offsets. That is the interleave this record proves byte-exactly.

Three lessons, all mechanical:

- **A snapshot copy of a script is not a snapshot** when the script resolves
  anything from its own location. It relocates the run instead of isolating it,
  and the relocation is SILENT because the fallback path existed.
- **Cancelling a task does not necessarily stop what it spawned.** Verify with a
  process query, not with the cancellation's return value.
- **Never reuse a log path across runs.** Every run gets a unique file; two
  writers at independent offsets produce a log that parses cleanly and describes
  nothing that happened.

This does NOT change the record's conclusions. Run B executed in the correct
tree; only its LOG is damaged (65 bytes overwritten at the seam, hence 33
surviving PASS lines for 77 claimed). §N1's run banner would have made all of
the above self-evident in one line, and remains the right fix.
