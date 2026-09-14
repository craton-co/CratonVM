# W8-E9-1 — three fixtures whose HotSpot oracle was broken, and the suite's denominator

Status: **closed for the three fixtures; instrument changes landed in
`regression-suite/run.sh`.** No VM defect is reported here, and none was found —
this record is entirely about the instrument and about three vectors that could
not be scored.

Subject: `regression-suite/src/RShutdownHooks.java`,
`regression-suite/src/RSimpleTimeZoneRaw.java`,
`regression-suite/src/RJdkStringCodePoints.java`, and the reporting half of
`regression-suite/run.sh`. Follows on from
[`W8-D2-1-two-summary-lines-and-the-suite-denominator.md`](W8-D2-1-two-summary-lines-and-the-suite-denominator.md),
whose §5 named the three fixtures and whose §7 specified N1/N2/N3.

Oracle: HotSpot **25.0.3+9** (Microsoft build 25.0.3+9-LTS, Windows), the same
JDK `run.sh` uses. This lane did not build or run CratonVM and did not need to:
every finding below is a fact about the *oracle* arm, which is `java` alone.

---

## Summary

**All three fixtures were logically correct on HotSpot all along.** Each exits 0,
each executes every assertion it claims to, and not one expectation is wrong.
What was wrong in all three was the **dialect they reported in**: they printed
their evidence on prefixes `run.sh`'s `extract()` filter deletes, and none of the
three printed the `PASS <Class>` banner `run.sh` greps for.

So their redness was not merely uninterpretable — for two of them it was
**guaranteed**, on any VM whatsoever, including a perfect one:

| fixture | guards that fired on HOTSPOT | would a correct CratonVM have passed? |
|---|---|---|
| `RShutdownHooks` | G1, **G3** | yes — it had a real `PASS` line |
| `RSimpleTimeZoneRaw` | G4 (masking a G1) | **no** — `run.sh` scores it `FAIL no PASS line` |
| `RJdkStringCodePoints` | G4, G2, G3 | **no** — same, and the diff compared two empty strings |

Measured, before the fix, by sourcing `harness-guard.sh` and running the real
`harness_guard_oracle` / `harness_guard_extract` against each oracle capture:

```
########## RShutdownHooks        rc=0
  [G1] the oracle printed 1 line(s) that extract() DELETES ...  | hook-stderr
  [G3] publishes no check count, and is not in harness-uncounted.txt
########## RSimpleTimeZoneRaw    rc=0
  [G4] the HotSpot oracle exited 0 but printed no 'PASS RSimpleTimeZoneRaw' line
########## RJdkStringCodePoints  rc=0
  [G4] the HotSpot oracle exited 0 but printed no 'PASS RJdkStringCodePoints' line
  [G2] nothing survives extract() — the cross-VM diff compares two empty strings
  [G3] publishes no check count ...
```

No assertion was weakened to make any of this green. Nothing in any of the three
expectation tables was touched.

---

## 1. `RJdkStringCodePoints` — the whole vector was invisible

The most serious of the three, and it is the W7-51/W7-60 vacuity shape caught on
a vector's first scheduled run.

The file printed `ok   <label>=<value>` per check and ended on
`@@RESULT checks=186 fails=0`. `extract()` keeps **only** lines prefixed `PASS `
or `CK `. So **187 of 187 lines were deleted** and the cross-VM key was the empty
string — G2's message is literal, not rhetorical: the diff compared two empty
strings, and an empty string always matches itself.

Why it did not read as a false green: `run.sh`'s own verdict also greps
`^PASS <Class>`, so the vector was scored `FAIL no PASS line`. The two defects
cancelled into a *permanent red* instead of a *permanent green*. That is luck,
not design — had the vector printed any `PASS RJdkStringCodePoints` line while
keeping its `ok` rows, it would have been a 186-check vector that could not fail,
scheduled and green forever.

**Fix.** The single `eq()` funnel now prints
`CK RJdkStringCodePoints <label>=<actual>`, so all 186 rows enter the diff, and
the two VMs compare **their own answers to each other** rather than each to this
file's expectation table. The tail is now
`CK … fails=N`, `CK … checks=N`, then `PASS RJdkStringCodePoints (186 checks)`
on the clean path only; `fails != 0` still throws before the banner.

`fails` and `checks` are on **separate lines** deliberately.
`harness_check_count` does `sub(/^.*checks=/, ""); print`, i.e. it takes the
whole rest of the line, so a combined `CK … checks=186 fails=0` yields the
"count" `186 fails=0`, and G3's `[ "$gc_count" -eq 0 ]` then fails as a *syntax*
error swallowed by its own `2>/dev/null` rather than comparing a number. One
value per line.

`@@RESULT` was dropped rather than kept alongside: it is on a deleted prefix, so
keeping it would re-fire G1 for one line. Checked before removing — nothing under
`regression-suite/` parses `@@RESULT`, and the runners that do
(`apps/hib-suite-runner/*`, `probes/*`) never run this class.

**After:** 189 raw lines, 189 surviving `extract()`, zero dropped, all four
guards silent, `rc=0`, `checks=186 fails=0`.

## 2. `RSimpleTimeZoneRaw` — the banner was in the wrong dialect

It printed `RESULT RSimpleTimeZoneRaw PASS`. `run.sh` and G4 both grep
`^PASS <Class>`; `extract()` deletes anything else. So a correct VM scored
`FAIL no PASS line`, the oracle was declared sick, and the word PASS never
reached the diff at all. G4 returns early, which is why the G1 that the same
line would have tripped never appeared in the log — **fixing only the missing
`PASS` line would have swapped one guard for another.** Both had to go in one
edit, and did.

**Fix:** `PASS RSimpleTimeZoneRaw (104 checks)` replaces the `RESULT` line; the
existing `CK RSimpleTimeZoneRaw checks=104` line is unchanged. Two lines, both
surviving, nothing dropped. 104 assertions, all holding on HotSpot.

## 3. `RShutdownHooks` — one dropped line, and a G3 the previous record missed

W8-D2-1 §3 attributed only G1 to this vector. **G3 also fired**, on every run,
and the reason is a spelling.

`harness_check_count` parses exactly two forms:

```awk
$0 ~ "^CK " cls " checks="            # CK RFoo checks=42
$0 ~ "^PASS " cls "( |$)" { match($0, /\(([0-9]+) checks?\)/) }   # PASS RFoo (42 checks)
```

The `PASS` arm **requires the parentheses**. This file printed
`PASS RShutdownHooks checks=4` — a count that looks published and is unreadable.
A census of every vector confirms this file was the **only** one of 77 counting
vectors using that spelling; the other 76 are already parenthesised. So this is a
one-file slip, not a systemic dialect split, and no ratchet entry is warranted.

G1's dropped line was `hook-stderr`, written by the hook to `System.err`. The
channel genuinely has to be exercised — `errOk` is what the fd1 line reports as
`err=ok` — but **a probe whose output is deleted proves nothing about the
channel**. The text now carries the prefix:
`CK RShutdownHooks hookErr ran=true`.

That is strictly stronger than the old pair. `err=ok` says only that the write
did not throw; the line landing says the bytes arrived. A VM whose `System.err`
is silently dead answers `err=ok` **and** loses the line, and the diff catches
it — the same "ran, output lost" vs "never ran" separation the vector already
built for `System.out` via the raw fd1 channel.

Ordering was checked, not assumed. The hook writes err → out → fd1, each flushed,
one thread, one merged stream (`run.sh` captures `2>&1`), so the interleave is
deterministic — and the vector *already* depended on this, since `hookOut`
(stdout) and `hookFd1` (raw fd 1) were already diffed as an ordered pair. Six
consecutive HotSpot runs are byte-identical (`md5sum` of `2>&1`), as are three
each of the other two fixtures.

**After:** five raw lines, five surviving, zero dropped, all guards silent.

## 4. End-to-end verification

The three fixtures were run through the **real `run.sh`**, in an isolated scratch
git repo, with a stand-in for `$CV` that forwards to HotSpot (this lane may not
execute the CratonVM binary; the stand-in exercises the *reporting*, which is
what was changed):

```
== RUN pid=552677 tree=…/scratchpad/e9/repo/regression-suite rev=1836433 suite=core scheduled=4 missing=0 ==
  RShutdownHooks PASS
  RSimpleTimeZoneRaw PASS
  RJdkStringCodePoints PASS
  RJdkHello      PASS
REGRESSION SUITE: 4 passed, 0 failed
  COUNTS: 4 of 4 SCHEDULED vectors passed; 0 scheduled vectors failed; 0 list/coverage errors (never scheduled); 0 harness-blindness flags (per-vector flags, not extra vectors).
```

All three now survive the full path including the cross-VM diff, with no guard
firing anywhere.

---

## 5. N1/N2/N3, as landed in `run.sh`

N1 and N2 are W8-D2-1 §7's literals verbatim. N3 was a judgement call and is
**applied**; the reasoning and the safety argument are below.

### N1 — the run banner

```
== RUN pid=$$ tree=$HERE rev=<short> suite=<core|jdk-only|all> scheduled=N missing=M ==
```

Printed before the binary check, so even a run that dies immediately identifies
itself. `tree=$HERE` is now meaningful because `ROOT` fails loudly (exit 3)
instead of falling back to a hard-coded path — the change that made W8-D2-1's
run A possible in the first place, already applied by the orchestrator and
deliberately left alone here.

### N2 — the `COUNTS:` decomposition

`total_vecfail=$total_fail` is snapshotted immediately before the list-hygiene
and harness folds; the final line then decomposes the accumulator into its four
populations. `total_pass + total_vecfail` is the scheduled denominator, and it is
now printed rather than inferred.

Known imprecision, restated from W8-D2-1 and not hidden: under `RELEASES=`, a
level whose compile failed adds one point that lands inside the `total_vecfail`
snapshot. With `RELEASES` unset — every invocation in this lane — the snapshot is
exactly the scheduled-vector failures.

### N3 — G2/G3 count only on a PASS. **Applied.**

The mechanism is confirmed by reading the code, not inferred: `run.sh` calls
`harness_guard_extract` unconditionally, after the verdict, and a vector that
throws before its banner leaves `cv.key` **empty by construction** — so G2 fires
(`nothing survives extract()`) and G3 fires with it (no count parses out of an
empty file). Every hard-failing vector not in `harness-uncounted.txt` therefore
manufactures a harness point as an arithmetic consequence of having failed. That
is what produced 9 duplicate points inside W8-D2-1's "21 failed" with zero
independent findings behind them.

G1/G4 keep counting unconditionally: "your ground truth is bad" is news whatever
CratonVM did, and §§1–3 above are exactly why — all three of this lane's
fixtures were found *by* an oracle-side guard.

**The safety property, verified rather than assumed.** N3 can only ever subtract
a point from a vector that is *already* contributing one through `$fail`, so
`total_fail` cannot reach zero via this branch and a run that should be red
cannot turn green. The suppressed direction is strictly "stop double-counting",
never "stop failing". The guard messages still print in full either way, so an
uncounted G2/G3 remains visible in the log.

Demonstrated against the real script, with `RJdkHello` forced to exit 1 with no
output (empty `cv.key` → G2+G3) and `RFsSingleton` scheduled with its broken
oracle (G4):

```
  RJdkHello      FAIL  cratonvm rc=1
  HARNESS ERROR [G2] RJdkHello: nothing survives extract() …
  HARNESS ERROR [G3] RJdkHello: publishes no check count …
  RFsSingleton   FAIL  no PASS line
  HARNESS ERROR [G4] RFsSingleton: … exited 0 but printed no 'PASS RFsSingleton' line.
  HARNESS ERROR [G2] RFsSingleton: nothing survives extract() …
  HARNESS ERROR [G3] RFsSingleton: publishes no check count …
  LIST ERROR: 'RJdkByteOrder' is in a class list but src/RJdkByteOrder.java does not exist
  HARNESS: 1 vector(s) reported on a comparison the suite cannot see: RFsSingleton
REGRESSION SUITE: 1 passed, 4 failed ( failed: RJdkHello RFsSingleton missing:RJdkByteOrder harness:RFsSingleton )
  COUNTS: 1 of 3 SCHEDULED vectors passed; 2 scheduled vectors failed; 1 list/coverage errors (never scheduled); 1 harness-blindness flags (per-vector flags, not extra vectors).
```

`RJdkHello`'s G2/G3 print and are **not** counted; `RFsSingleton`'s oracle-side
G4 **is**. Pre-N3 the same run reads `5 failed`. Both decompositions close:
1+2 = 3 scheduled, and 2+1+1 = 4.

---

## 6. `--synthetic-jdk` and `RJdkOptionalShape` — judgement, and a declined suggestion

Asked of this lane mid-task: `RJdkOptionalShape` is registered in
`CORE_CLASSES`, which runs default mode, but lane E2 measured that
`native-builtins/src/http2.rs`'s nine `Optional`-minting sites answer **only**
under `--synthetic-jdk`; in default mode `net_phase_e.rs`'s `re5_optional`
answers and is already correct. Options offered were (a) add a `--synthetic-jdk`
arm to the suite, or (b) note the limitation at the registration.

**Verdict: (b) now, with the note landed in `run.sh`; (a) is a nomination with
constraints, not something to land blind.** Reasons, in order of weight:

1. **This is a mislabelling, not a vacuity.** The vector's own "Mode
   independence" section asserts `http2.rs` "is registered in both arms" — a
   stated premise now measured false, so the guard it scopes is only as good as
   the premise. But `core()`, `prim()`, `stream()`, `version()`, `process()` and
   `misc()` are real coverage of `re5_optional` and of the class library and go
   red if either regresses. Only `httpmint()` is inert. What is untrue is the
   *claim* that a green run says anything about `http2.rs`. Unscheduling the
   vector would delete working coverage to solve a documentation problem.
2. **An arm would be a mechanism nobody here can execute.** `--synthetic-jdk` is
   a runtime mode requiring a binary built with `--features synthetic-jdk`; a
   stock build refuses the flag and exits 1. An arm that skips when the binary
   cannot take it is a gate that cannot fail — the precise defect the rest of
   this record is about. An arm that hard-fails instead turns every one of eight
   live lanes red until a second binary exists.
3. **Landing it default-off would be the recorded anti-pattern.** The only
   version I could land and not test is one with an empty class list, i.e. a
   dormant mechanism recorded as done. This project has a name for that.

If (a) is wanted, the shape that cannot lie: an opt-in `CV_SYNTHETIC=<path>`
plus a `SYNTHETIC_CLASSES` list, where a **non-empty list with no usable
`CV_SYNTHETIC` is a hard failure** (the same doctrine as `missing:` — a
registration with no way to execute it is red, never skipped), and where the
binary refusing `--synthetic-jdk` is likewise red. It must be landed by a lane
that can build both binaries and can watch the arm go red-then-green.

**Declined: folding "scheduled but structurally incapable of failing" into
`COUNTS:`.** `COUNTS:` decomposes numbers the script *computes*; incapacity is
not computable from a schedule, which is why G2/G3 approximate it from a
vector's **output** instead. A hand-maintained list of non-discriminating
vectors added as a fifth term would add a fourth incommensurable population to
the one line whose entire purpose is to stop summing incommensurable
populations — undoing N2 in the act of extending it. If the concept is wanted it
belongs as a **G5 guard with a two-way ratchet file**, on the
`harness-uncounted.txt` model, where clearing an entry is what makes the run
green again. That is a separate piece of work with a separate mutation check.

Corroboration worth keeping, from lane E2: `http2.rs` carries 64 unit tests,
every one of them registration-only (`find(...).is_some()`), and all 64 stayed
green through the entire `Optional` layout defect. Coverage of a surface is not
coverage of a contract — the same distinction this record draws four more times.

---

## 7. Nominations (outside this lane's ownership)

### NOM-1 — `RFsSingleton` is a fourth broken oracle, and it is scheduled

`regression-suite/src/RFsSingleton.java` (in `CORE_CLASSES`) has **exactly** the
`RJdkStringCodePoints` shape: measured on HotSpot 25.0.3+9 it exits 0 and prints
13 lines, **0** of them `PASS `/`CK `-prefixed (`ok fs.defaultStable`,
`ok fs.defaultPathsGet`, …). G4 + G2 + G3 all fire on the oracle and `run.sh`
scores it `FAIL no PASS line` on any VM including a correct one — verified live
in §5's scenario B. It was not in W8-D2-1's list because it was registered after
that run. The repair is §1's, applied to that file: prefix the per-check funnel
with `CK RFsSingleton `, and end on `PASS RFsSingleton (N checks)`. Owner: the
lane that owns `RFsSingleton.java`.

### NOM-2 — `RServiceLoaderDoubleSource` is registered nowhere

`regression-suite/src/RServiceLoaderDoubleSource.java` exists, compiles, and
appears in no class list and in no `UNREGISTERED_CLASSES` row, so it produces a
`COVERAGE WARNING` on every run and would be **fatal under `STRICT_COVERAGE=1`,
which this file's own comment says CI should set**. It prints
`PASS RServiceLoaderDoubleSource (` … so it is a finished vector that simply
never runs — the `RJdkPhaser` shape the census exists to catch. Its owning lane
should add it to `CORE_CLASSES`/`JDKONLY_CLASSES` (run.sh is E9-owned; send the
registration here) or give it an `UNREGISTERED_CLASSES` row with a reason.

### NOM-3 — a `--synthetic-jdk` arm, per §6

Design constraints in §6. Needs a lane that can build both binaries.

### NOM-4 — `harness_check_count`'s `PASS` arm accepts only the parenthesised form

Not a defect, but it silently ignored a published count for a year (§3). A
one-line widening of the `awk` to also accept `PASS <Class> checks=N` would make
the two spellings symmetric with the `CK` arm. **Recommended against** on
balance — 76 of 77 vectors already use the parenthesised form, and widening a
parser so a slip stops being visible is the wrong direction — but recorded so
the next person who hits it finds this note instead of re-deriving it.
`harness-guard.sh` is not this lane's file either way.

---

## 8. What the summary line looks like now

Every run gains one line at the top and one at the bottom:

```
== RUN pid=<pid> tree=<abs path to regression-suite> rev=<short sha> suite=<core|jdk-only|all> scheduled=<N> missing=<M> ==
…
REGRESSION SUITE: <P> passed, <F> failed [( failed: … )]
  COUNTS: <P> of <P+V> SCHEDULED vectors passed; <V> scheduled vectors failed; <L> list/coverage errors (never scheduled); <H> harness-blindness flags (per-vector flags, not extra vectors).
```

with `F = V + L + H` and `P + V = scheduled`. The `REGRESSION SUITE:` line is
unchanged and still must not be quoted as a pair; the `COUNTS:` line beneath it
is the quotable one.

Two `run.sh` processes sharing a log are now trivially separable by `pid=`, and a
`missing:` list is attributable to a tree and a revision — which is the entire
content of W8-D2-1's audit, reduced to one line printed for free.
