# W8-E15-1 — the fourth broken oracle, the vector that was registered nowhere, and G5: a ratchet for reach

Status: **closed for `RFsSingleton` and `RServiceLoaderDoubleSource`; G5 landed
and mutation-checked.** No VM defect is reported here and none was looked for —
this record is entirely about the instrument.

Subject: `regression-suite/src/RFsSingleton.java`,
`regression-suite/src/RServiceLoaderDoubleSource.java`,
`regression-suite/run.sh`, `regression-suite/harness-guard.sh`.

Follows on from
`W8-E9-1-three-broken-oracles-and-the-suite-denominator.md` (retired: `W8-E9-1-three-broken-oracles-and-the-suite-denominator`)
— its §7 NOM-1 and NOM-2 are §§1–2 below, and its §6's declined suggestion is §3
— and from
[`D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md`](D1-R11-SERVICELOADER-DOUBLE-SOURCE-20260813.md),
whose §8 NOM 5 specified the wiring in §2.

Oracle: HotSpot **25.0.3+9** (Microsoft build 25.0.3+9-LTS, Windows), the same
JDK `run.sh` uses. This lane did not build or run CratonVM. Every number below
is `java`/`javac` on that oracle, or `bash` against the real `run.sh` with a
stand-in `$CV` that forwards to HotSpot, in an isolated scratch repository.

---

## Summary

| | before | after |
|---|---|---|
| `RFsSingleton` on HotSpot | rc=0, 13 lines, **0** survive `extract()`, G4+G2+G3 fire, `run.sh` scores it `FAIL no PASS line` **on any VM** | rc=0, 15 lines, **15** survive, 0 dropped, all five guards silent, count parses as 12 |
| `RServiceLoaderDoubleSource` | compiles, in no list, `COVERAGE WARNING` every run, **fatal under `STRICT_COVERAGE=1`**, 0 checks ever executed | scheduled in `JDKONLY_CLASSES`, wired, **PASS (1266 checks)** through the real `run.sh` |
| mislabelled reach | recorded in a comment nobody can test | **G5**, a two-way ratchet over a four-row table, 7 mutations all firing |

No assertion was weakened anywhere. `RFsSingleton` still runs the same twelve
checks including its two negative controls, and the mutation control below shows
it still goes red.

---

## 1. `RFsSingleton` — the fourth broken oracle, and it was scheduled

`CORE_CLASSES` contained it; measured on the oracle it printed

```
ok fs.defaultStable
… 11 more `ok` lines …
@@RESULT checks=12 fails=0
```

Thirteen lines, **none** of them on a prefix `extract()` keeps, and no
`PASS RFsSingleton` line anywhere. That is `RJdkStringCodePoints`'s shape
exactly (W8-E9-1 §1): G4 fires (oracle exited 0 with no banner), G2 fires
(nothing survives, so the cross-VM diff compares two empty strings), G3 fires
(no parseable count), and `run.sh`'s own verdict greps `^PASS RFsSingleton` and
scores it `FAIL no PASS line` **on a correct VM**. It post-dates W8-D2-1's audit,
which is why it was not in that list.

**Fix**, following the sibling repair rather than inventing a second dialect:

* the funnel is now `check()` plus a `checkStr()` twin, and both print
  `CK RFsSingleton <name>=<ACTUAL>` **unconditionally**. The value is the VM's
  own answer, never this file's expectation, so the two VMs compare their
  answers *to each other*. A failed check adds a second line,
  `CK RFsSingleton FAILED <name> expected=<expected>`, and increments `fails`.
* `prov.scheme` had an open-coded `checks++`/`if` block; it is now the one
  `checkStr()` call, and it publishes the string — `prov.scheme=file`. That
  matters: the defect this row exists for answered **`jrt`** for the platform
  file system, and a boolean would have told the reader only that something was
  wrong.
* the tail is `CK RFsSingleton fails=0`, then `CK RFsSingleton checks=12`, then
  `PASS RFsSingleton (12 checks)` **on the clean path only**; `fails != 0` still
  throws before the banner.
* `@@RESULT` was **dropped**, not kept alongside — it is on a deleted prefix, so
  keeping it would re-fire G1 for one line. Checked before removing: nothing
  under `regression-suite/` parses it (the only two `@@RESULT` hits in `src/`
  are comments in `RJdkStringCodePoints` recording the same removal), and the
  runners that do parse it never run this class.

`fails` and `checks` are on **separate lines** deliberately, for the reason
W8-E9-1 §1 established: `harness_check_count` does
`sub(/^.*checks=/, ""); print`, so a combined line yields a non-numeric "count"
and G3's `[ "$gc_count" -eq 0 ]` then errors out inside its own `2>/dev/null`
instead of comparing a number. **That trap is live elsewhere right now** — see
NOM-3.

**Measured, after:**

```
CK RFsSingleton fs.defaultStable=true
CK RFsSingleton fs.defaultPathsGet=true
CK RFsSingleton fs.defaultPathOf=true
CK RFsSingleton fs.defaultGetPath=true
CK RFsSingleton fs.viaUri=true
CK RFsSingleton prov.stable=true
CK RFsSingleton prov.scheme=file
CK RFsSingleton prov.installedIsDefault=true
CK RFsSingleton prov.installedStable=true
CK RFsSingleton prov.viaPath=true
CK RFsSingleton neg.pathsDistinct=false
CK RFsSingleton neg.objectsDistinct=false
CK RFsSingleton fails=0
CK RFsSingleton checks=12
PASS RFsSingleton (12 checks)
```

rc=0; 15 raw lines, 15 through `extract()`, **zero dropped**;
`harness_guard_oracle` and `harness_guard_extract` both return 0 with an empty
message buffer; `harness_check_count RFsSingleton` returns `12`; five
consecutive runs are `md5sum`-identical on the merged `2>&1` stream. It is also
`1 vectors sound, 0 flagged` under `harness-selfcheck.sh`.

**Mutation control**, because a repaired reporter that can no longer fail is the
defect one level up. Flipping `neg.objectsDistinct`'s expectation to `true` in a
scratch copy:

```
CK RFsSingleton neg.objectsDistinct=false
CK RFsSingleton FAILED neg.objectsDistinct expected=true
CK RFsSingleton fails=1
CK RFsSingleton checks=12
Exception in thread "main" java.lang.RuntimeException: 1 default-filesystem singleton checks failed
rc=1
```

— the value line still prints (so the diff sees the disagreement even on a VM
that swallowed the throw), no `PASS` banner is emitted, and rc is non-zero.

## 2. `RServiceLoaderDoubleSource` — wired and scheduled, not shelved

It compiled, appeared in **no** class list and in **no** `UNREGISTERED_CLASSES`
row, warned on every run, and would have been **fatal under
`STRICT_COVERAGE=1`, which `run.sh`'s own documentation says CI should set.**
1,523 checks in its author's configuration, 8 mutation controls, and it catches
a state HotSpot cannot be put into — sitting at exactly zero executions.

Its author left it unscheduled for a good reason, restated here because it is
the whole design: its discriminating check needs a **modular artefact on the
class path when the VM starts** (CratonVM scans the app class path for
`module-info.class` inside `ClassManager::new`, before `main`, so a jar the
vector builds itself would be scanned by nobody), and when that input is absent
the vector **fails rather than skips**. A precondition that silently disarms the
only discriminating check is how a gate becomes green-forever.

**Verdict: supply the entry and schedule it.** Three pieces landed in `run.sh`,
in the order the file already prescribes for `RPriorityQueueGc` — wiring first,
registration second:

1. `CPSEP`, derived from `uname -s`. New and load-bearing: this file had never
   needed a class-path separator, and it is `;` on Windows and `:` on the Linux
   build host, which `$JDK` cannot distinguish.
2. `class_cp_extra()`, a per-class hook appending `$CPSEP$MODBUILD/$JDKONLY_MODULE`
   — the exploded module `regression-suite/modules/` already compiles for
   `RJdkModule`. Applied at **both** launch sites, so **both VMs** get it.
3. a `class_args()` arm handing both VMs
   `-Dcratonvm.rt.cpmodule` / `.cpclass` / `.cpservice`. `-D` is parsed by
   CratonVM's launcher too (`vm-cli/src/main.rs`, `strip_prefix("-D")` into the
   system-property list), so the oracle and the VM see the identical
   configuration. That arm must **never** grow a `--module-path`: the vector
   asserts `System.getProperty("jdk.module.path") == null` as a precondition,
   because a module-path module *is* resolved into the boot layer on a real JVM
   and the vector would then be measuring its own command line.

Registered in **`JDKONLY_CLASSES`**, per D1's NOM 5 and because that is the mode
the defect was measured in. Deliberately **not** given a `--jdk-only` pin in
`class_cv_args`: its discriminating assertion is
`ModuleLayer.boot().findModule(<a -cp-only module>)` and the promotion it
catches (`populate_boot_layer_modules`) is unconditional, so under `SUITE=all`
with no `CRATONVM_ARGS` the vector still asks a real question about Compatible
mode. Pinning the flag would hide whether the promotion happens there too.

**Measured on the oracle in exactly the scheduled configuration:**

```
CK RServiceLoaderDoubleSource getResources names=7 iters=64 dup=0 drift=0
CK RServiceLoaderDoubleSource setDiscipline hashset=4 linked=64
CK RServiceLoaderDoubleSource providers services=6 iters=16 dup=0 drift=0 cached=6
CK RServiceLoaderDoubleSource separation inBootLayer=false named=false svc=on
CK RServiceLoaderDoubleSource checks=1266
PASS RServiceLoaderDoubleSource (1266 checks)
```

rc=0, 6 lines out, 6 through `extract()`, `md5sum`-identical over three runs, all
guards silent, and green through the real `run.sh` both under `ONLY=` and under
the `SUITE=jdk-only` schedule.

**Why 1266 and not D1's 1523 or 754, and the caution that follows.** The count
scales with how many URLs `getResources` finds per descriptor name, times the
iteration count — so it is a function of the class path, not a literal. It is
identical on both arms of one run, which is all the cross-VM diff and G3 need,
but **nobody should ratchet on the number**. Quote the configuration with it.

**One cost, measured rather than discovered later.** `harness-selfcheck.sh` runs
the same class lists (it `sed`s them out of `run.sh`) but has no equivalent of
`class_args`/`class_cp_extra` — it hard-codes one special case for `RJdkModule`.
So as of this change it reports:

```
  HARNESS ERROR [G4] RServiceLoaderDoubleSource: the HotSpot oracle run FAILED (rc=1) …
  HARNESS ERROR [G3] RServiceLoaderDoubleSource: publishes no check count …
HARNESS SELF-CHECK: 1 vectors sound, 1 flagged (RServiceLoaderDoubleSource)
```

That flag is **accurate** — from that script's configuration the vector really
cannot run — and its remedy is two lines in a file this lane does not own. It is
NOM-1, and it should land in the same wave. The alternative was to leave the
wiring in `run.sh` unreachable, i.e. a dormant mechanism recorded as done, which
this project has a name for.

## 3. G5 — a ratchet for reach, and why it is not a fifth term on `COUNTS:`

W8-E9-1 §6 declined to fold "scheduled but structurally incapable of failing"
into the `COUNTS:` line, on the grounds that a hand-maintained fifth term would
add a fourth incommensurable population to the one line whose purpose is to stop
summing incommensurable populations. That reasoning is preserved: **G5 is its
own guard, and its findings are counted into the existing harness-flag
population**, `$total_hbad`. `COUNTS:` still has four terms and still closes.
A G5 finding is a per-vector instrument flag on a scheduled vector, which is
precisely what that fourth term already says it holds.

### 3.1 The motivating case, and the distinction the guard must express

`RJdkOptionalShape` is in `CORE_CLASSES`, i.e. default mode. Lane E2 measured
that the nine `Optional`-minting sites in `native-builtins/src/http2.rs` answer
**only** under `--synthetic-jdk`; in default mode `net_phase_e.rs`'s
`re5_optional` answers and is already correct. So its `httpmint()` block — the
only block written to reach C12-3 — passes for reasons unrelated to the defect
it names.

**But its six other families are real default-mode coverage that goes red if the
class library or `re5_optional` regresses.** This is a MISLABELLING of one
family, not a vacuous fixture. A guard whose only verdict is "this vector cannot
fail" would be wrong about `RJdkOptionalShape` and would be argued with instead
of fixed. So **the unit is the family, not the vector.**

### 3.2 The table and the two directions

`HARNESS_NONDISCRIMINATING` in `run.sh`, one row per adjudication:

```
<Class>|<INERT|LIVE>|<family>|<mode>|<reason>
```

`INERT` = one named family targets code that only runs in `<mode>`, which is not
the mode the vector is scheduled in. `LIVE` = the source *names* `<mode>` — which
is what the ADD scan keys on — and has been adjudicated as genuinely
discriminating where it is scheduled. Four rows today: `RJdkOptionalShape`
(INERT, `httpmint`), and `RBlockingQueue`, `RDirectBufferElem`, `RJdkByteOrder`
(all LIVE, each with the reason in the row).

`harness_guard_nondiscriminating` in `harness-guard.sh` makes both directions
loud:

* **ADD** — a listed vector whose source names the mode and has **no** row.
  Adding a mode-straddling fixture forces an explicit INERT/LIVE verdict.
* **STALE** — a row whose class no longer exists, is no longer in any class
  list, whose INERT family is no longer named in the source, whose LIVE source
  no longer mentions the mode, or — the repair signal — **whose mode the run is
  actually executing**, at which point the family is live and the row must go.

Clearing an entry is what makes the run green again, exactly as in
`harness-uncounted.txt`.

**Scope is argued, not assumed.** The ADD scan looks for one token,
`synthetic-jdk`. `--synthetic-jdk` needs a binary built with
`--features synthetic-jdk`; a stock build refuses the flag and exits 1, so
nothing this suite can do supplies it and a family whose target only answers
there is *structurally* incapable of failing here. `--jdk-only`, by contrast, is
a flag the harness **can** supply and does supply per class
(`class_cv_args`'s `RJdkSqlPackage` arm) — a family needing it is misconfigured,
not inert, the fix is one line, and folding it in would put most of the `RJdk*`
corpus into a table that has to stay small enough to re-read.

The ADD scan runs over `LISTED_CLASSES` rather than over the classes a given
invocation scheduled, so G5's verdict is the same under `SUITE=core`,
`SUITE=jdk-only` and `SUITE=all`. A ratchet whose answer depends on the
invocation is one every lane learns to attribute to the invocation.

### 3.3 Mutation matrix — 7 mutants, 7 fires, pristine silent

| # | mutation | result |
|---|---|---|
| M0 | pristine tree | **silent**, `bad=0` |
| M1 | `RBlockingQueue` row deleted | **ADD** on `RBlockingQueue` |
| M2 | row's class renamed to a non-existent source | **STALE** "src/… does not exist" (+ ADD on the now-unadjudicated real class) |
| M3 | row added for a class in no class list | **STALE** "is in no class list, so there is no schedule to mislabel" |
| M4 | `httpmint` → `httpmintXX` in the INERT row | **STALE** "family … is no longer named in src/RJdkOptionalShape.java" |
| M5 | `CRATONVM_ARGS="--synthetic-jdk"` | **STALE** "this run executes --synthetic-jdk, so 'httpmint' IS discriminating here" |
| M6 | LIVE row repointed at a source with no mode mention | **STALE** "no longer names a non-default runtime mode" |
| M7 | verdict `LIVE` → `MAYBE` | **malformed row** |

End-to-end through the real `run.sh` (M1), showing the counting:

```
  HARNESS ERROR [G5] RBlockingQueue: scheduled, and its source names a runtime mode (synthetic-jdk)
    that no run of this suite can execute — a stock binary refuses --synthetic-jdk. Say which: …
  HARNESS: 1 vector(s) reported on a comparison the suite cannot see: RBlockingQueue
REGRESSION SUITE: 2 passed, 1 failed ( failed: harness:RBlockingQueue )
  COUNTS: 2 of 2 SCHEDULED vectors passed; 0 scheduled vectors failed; 0 list/coverage errors (never scheduled); 1 harness-blindness flags (per-vector flags, not extra vectors).
```

exit 1, and both decompositions close: `1 = 0+0+1` and `2+0 = 2` scheduled.

### 3.4 What G5 cannot do, stated so it is not quoted for more

**A family that is inert and whose source says nothing about the mode is not
detectable here — and the motivating row is exactly that case.**
`RJdkOptionalShape.java` never writes `synthetic-jdk`; its "Mode independence"
section asserts the opposite, and it was caught by measuring which Rust file
answers, not by reading the fixture. The ADD scan catches the syntactically
visible half; the table carries the rest by hand.

That is the same residual blindness G3 has, and it is the reason G5 is a
**ratchet rather than a census**. The alternative — a guard that claims to
enumerate every non-discriminating family — would be a gate that measures a
fraction and reads as good news.

One implementation note worth keeping: the row loop reads from a **here-doc**,
never a pipe. `... | while read` runs the loop in a subshell, every counter is
discarded on exit, and the guard silently never fires — the same shape
`prune_missing`'s comment in `run.sh` warns about.

---

## 4. What the suite output looks like now, and whether the arithmetic closes

Unchanged in shape from W8-E9-1 §8: one `== RUN … ==` banner at the top, the
`REGRESSION SUITE:` line, and the four-term `COUNTS:` line beneath it. **G5 adds
no line to `COUNTS:` and no new population**; when it fires it prints
`HARNESS ERROR [G5] <class>: …` in the list-hygiene block and its points join
the existing `HARNESS:` line. The only other text change is that the `HARNESS:`
remediation now names *which* table to edit, because it previously pointed every
guard at `harness-uncounted.txt` and G5's table is elsewhere.

Measured against the real `run.sh` with the HotSpot stand-in, in an isolated
scratch repo:

```
SUITE=jdk-only   37 passed, 0 failed
  COUNTS: 37 of 37 SCHEDULED vectors passed; 0 scheduled …; 0 list/coverage …; 0 harness-blindness flags.

SUITE=core       57 passed, 2 failed ( failed: harness:RArrayStoreTiers harness:RArrayStoreInterfaces )
  COUNTS: 57 of 57 SCHEDULED vectors passed; 0 scheduled vectors failed; 0 list/coverage errors (never scheduled); 2 harness-blindness flags (per-vector flags, not extra vectors).
```

`RFsSingleton PASS`, `RServiceLoaderDoubleSource PASS` and `RJdkIntrinsics3 PASS`
in their scheduled positions. Both decompositions close: `P + V = scheduled`
(57+0, 37+0) and `F = V + L + H` (2 = 0+0+2, 0 = 0+0+0). The two `SUITE=core`
flags are **pre-existing G1 findings that this lane did not create and does not
own** — NOM-2. The one remaining `COVERAGE WARNING` is NOM-3.

Scheduled denominators: `CORE_CLASSES` 56 → 57 (`RJdkIntrinsics3`, §4.1),
`JDKONLY_CLASSES` 36 → 37 (`RServiceLoaderDoubleSource`, §2), so `SUITE=all`
goes 92 → 94.

### 4.1 `RJdkIntrinsics3` — registered on request from lane E10, verified first

E10 sent the registration mid-task (`run.sh` is this lane's file). Applied to
`CORE_CLASSES`; no `class_args`/`class_cv_args`/`class_cp_extra` hook needed.

**Verified independently before landing it**, because scheduling a vector that
does not compile breaks the suite compile for every lane, and because this file
had in fact been mid-write earlier in the session — it failed `javac` at 05:26
and 05:39 with `cannot find symbol` on eight and then five not-yet-written
methods, which is a shared-worktree race, not a defect, and it is why the
verification is recorded rather than taken on report:

* `javac` clean; `java` rc=0
* **52 raw lines, 52 through `extract()`, zero dropped** — so G1 is silent
* `harness_guard_oracle` and `harness_guard_extract` both return 0 with an empty
  message buffer; `harness_check_count` returns **1011**
* `md5sum`-identical over three runs
* green in place: `RJdkIntrinsics3 PASS` in the full `SUITE=core` run above

**G5 interaction, checked rather than assumed:** its source contains no
`synthetic-jdk` mention, so the ADD scan does not require a row and G5 stays
silent (`bad=0`) with it scheduled. Which is the right answer — E10's account of
it (16 families, 16 separate processes with isolated counts summing to 1011,
16/16 mutation controls each encoding a specific wrong theory) describes the
**opposite** of what G5 exists to name, and it is the best positive example in
the tree of a vector that can discriminate.

Two operational notes from E10 worth preserving here because they are about
*how the vector must be driven*, not about its content: the per-family
`--only=<f>` loop must run **before** any aggregate run, since a VM abort
truncates the aggregate and destroys the other 15 families' results; and the
fixture prints a `<family>-step=<call>` breadcrumb before every panic candidate,
so the last stdout line names the killing call. On HotSpot the last breadcrumb
is `CK RJdkIntrinsics3 mathexact-step=Math.toIntExact(Long.MIN_VALUE)` with
rc=0 — a breadcrumb reached, not an abort. E10's falsifiable prediction stands
on the record: `mathexact` should abort at
`mathexact-step=Math.negateExact(Integer.MIN_VALUE)` **or not at all**; an abort
anywhere else falsifies its risk model and is a finding.

---

## 5. Nominations

### NOM-1 (blocking, pairs with §2) — `harness-selfcheck.sh` needs the two hooks

It reads the class lists out of `run.sh` but launches every vector with a bare
`-cp "$WORK/cls"` and one hard-coded `RJdkModule` special case, so the
newly-scheduled `RServiceLoaderDoubleSource` is flagged G4+G3 there. The change
mirrors the line already present. Replace

```sh
  extra=""
  [ "$c" = RJdkModule ] && [ -n "$HAVE_MODULE" ] && extra="--module-path $WORK/mod --add-modules $JDKONLY_MODULE"
  timeout "$TIMEOUT" "$HS" $extra -cp "$WORK/cls" "$c" > "$WORK/out/$c.raw" 2>&1
```

with

```sh
  extra=""; cpx=""
  [ "$c" = RJdkModule ] && [ -n "$HAVE_MODULE" ] && extra="--module-path $WORK/mod --add-modules $JDKONLY_MODULE"
  # Mirrors run.sh's class_args + class_cp_extra for the one vector that needs a
  # MODULAR artefact on the CLASS path (never on the module path — the vector
  # refuses that configuration on purpose). CPSEP is `;` on Windows, `:` else.
  if [ "$c" = RServiceLoaderDoubleSource ] && [ -n "$HAVE_MODULE" ]; then
    case "$(uname -s 2>/dev/null)" in MINGW*|MSYS*|CYGWIN*|*NT-*) sep=';' ;; *) sep=':' ;; esac
    cpx="$sep$WORK/mod/$JDKONLY_MODULE"
    extra="$extra -Dcratonvm.rt.cpmodule=$JDKONLY_MODULE -Dcratonvm.rt.cpclass=com.cratonvm.jdkonly.svc.Greeter -Dcratonvm.rt.cpservice=com.cratonvm.jdkonly.svc.Greeter"
  fi
  timeout "$TIMEOUT" "$HS" $extra -cp "$WORK/cls$cpx" "$c" > "$WORK/out/$c.raw" 2>&1
```

Better still, since the two files now duplicate three hooks: move
`class_args`/`class_cv_args`/`class_cp_extra` into `harness-guard.sh`, which both
already source, and delete both copies. That is the same argument
`harness-guard.sh`'s header makes for `extract()` having exactly one definition.

### NOM-2 — `RArrayStoreTiers` and `RArrayStoreInterfaces` are broken oracles five and six

Both are in `CORE_CLASSES`; measured on HotSpot 25.0.3+9 they exit 0, print
their `PASS` banner (so `run.sh` scores them PASS) and drop **17** and **29**
lines respectively before the diff:

```
  | RArrayStoreTiers ITERS=3000  (C1 threshold 500; run this both with and without --nojit)
  | s01 String[] as Object[] <- Integer   cold=[ArrayStoreException] hot=[ArrayStoreException]
  | s02 Comparable[] as Object[] <- Object cold=[ArrayStoreException] hot=[ArrayStoreException]
  …
```

This is the weaker G1 shape, not §1's, but it is the **whole per-row cold/hot
evidence** of two tier-parity vectors: the harness sees only the banner, so a VM
that threw the wrong exception on 28 of 29 rows and the right one on all 29
produce the identical extract. The repair is §1's: prefix each row
`CK <Class> <key>=<value>`. Owner: whoever owns those two files.

### NOM-3 — `RSslNullSession` is unregistered **and** has the combined-count line

`src/RSslNullSession.java` is in no class list and in no `UNREGISTERED_CLASSES`
row — the `RJdkPhaser` shape again, `COVERAGE WARNING` on every run and fatal
under `STRICT_COVERAGE=1`. It is finished: it prints `CK` rows and
`PASS RSslNullSession (N checks)` at line 193.

But line 189 is `CK RSslNullSession checks=<n> failures=<m>`, and that is the
combined-line trap W8-E9-1 §1 documented. Measured:

```
$ printf 'CK RSslNullSession checks=189 failures=0\n' | harness_check_count RSslNullSession
189 failures=0
```

The `CK` arm matches first and `exit`s, so the parenthesised `PASS` count is
never reached; G3's `[ "$gc_count" -eq 0 ]` then errors inside its own
`2>/dev/null` and the guard silently no-ops. **Split the line before scheduling
it**, or the vector arrives with an unreadable count and a guard that cannot say
so. Owner: whoever owns that file; the registration is a word in `run.sh` and
can be sent here.

### NOM-4 — ~~`RJdkIntrinsics3` is unregistered~~ **LANDED, §4.1**

Registered in `CORE_CLASSES` on lane E10's request during this task, after
independent verification on the oracle. No longer a nomination.

### NOM-5 — index the new record

`docs/known-issues/jdk-only/INDEX.md` is not this lane's file; add a row for
this record next to W8-E9-1's.

### NOM-6 (declined here, restated) — a `--synthetic-jdk` arm

Unchanged from W8-E9-1 §6/NOM-3: it needs a lane that can build both binaries.
What is new is that G5 now makes the *consequence* of not having one visible on
every run instead of resting in a comment, and that when such an arm lands, G5's
M5 direction turns the `RJdkOptionalShape` row red until it is deleted — which
is the ratchet working, not a regression.

---

## 6. The lesson

**A repaired reporter must be mutation-checked, not just re-read.** Every fix in
§1 makes a vector print *more*, and "prints more" and "can still fail" are
different properties: `RFsSingleton`'s repair would have looked identical if the
throw had been moved after the banner. One `sed` on a scratch copy answers it.

**And the sharper one: the unit of vacuity is the family, not the vector.** The
suggestion this lane inherited was a list of vectors that cannot fail. The one
concrete case is a vector six-sevenths of which is load-bearing, and a guard
built at vector granularity would have demanded deleting that coverage to fix a
sentence in a Javadoc comment. Getting the granularity wrong does not make a
guard noisy in a way people fix — it makes it a guard people learn to disagree
with.
