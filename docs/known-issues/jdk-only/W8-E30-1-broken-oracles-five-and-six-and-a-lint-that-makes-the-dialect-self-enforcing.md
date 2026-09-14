# W8-E30-1 — broken oracles five and six, the shared launch hooks, and G6: a lint that makes the reporting dialect self-enforcing

Status: **closed for `RArrayStoreTiers`, `RArrayStoreInterfaces` and
`RSslNullSession`; the launch hooks are shared and ratcheted; G6 landed and
mutation-checked (8 mutants, 8 fires).** No VM defect is reported here and none
was looked for — this record is entirely about the instrument. **G6 found an
eighth fixture in the same dialect (`RJdkProcess`) which this lane does not own;
that is NOM-1 and it is blocking for `SUITE=jdk-only` / `SUITE=all`.**

Subject: `regression-suite/src/RArrayStoreTiers.java`,
`regression-suite/src/RArrayStoreInterfaces.java`,
`regression-suite/src/RSslNullSession.java`,
`regression-suite/harness-guard.sh`, `regression-suite/harness-selfcheck.sh`.

Follows on from
[`W8-E15-1-the-fourth-broken-oracle-the-unscheduled-vector-and-the-reach-ratchet.md`](W8-E15-1-the-fourth-broken-oracle-the-unscheduled-vector-and-the-reach-ratchet.md)
— its NOM-1 is §3 below, its NOM-2 is §1, its NOM-3 is §2 — and from
`W8-E9-1-three-broken-oracles-and-the-suite-denominator.md` (retired: `W8-E9-1-three-broken-oracles-and-the-suite-denominator`),
whose NOM-4 is the standing judgement §4 addresses.

Oracle: HotSpot **25.0.3+9** (Microsoft build 25.0.3+9-LTS, Windows), the same
JDK `run.sh` uses. This lane did not build or run CratonVM. Every number below
is `java`/`javac` on that oracle, or `bash` against the real `run.sh` /
`harness-selfcheck.sh` with a stand-in `$CV` that forwards to HotSpot, in an
isolated scratch repository.

---

## Summary

| | before | after |
|---|---|---|
| `RArrayStoreTiers` | rc=0, 18 lines, **1** survives `extract()`, **17 dropped** | rc=0, 35 lines, **35** survive, **0 dropped**, count parses as 63 |
| `RArrayStoreInterfaces` | rc=0, 30 lines, **1** survives, **29 dropped** | rc=0, 59 lines, **59** survive, **0 dropped**, count parses as 108 |
| `RSslNullSession` | `checks=47 failures=0` on ONE line → `harness_check_count` returns the string `47 failures=0` and G3 silently no-ops | two lines, count parses as 47, ready to schedule |
| launch hooks | two definitions (`run.sh` + a hard-coded `RJdkModule` line in the selfcheck), already diverged | ONE definition in `harness-guard.sh`, and **H1** holds `run.sh`'s remaining copies behaviourally identical over a 1,140-row matrix |
| the dialect | a contract in a `grep` expression and three records | written down in `harness-guard.sh`, and **G6** enforces it |
| near-miss census | 7 known offenders, found one at a time by hand | **8** — G6 found `RJdkProcess` on its first run over the corpus |
| `harness-selfcheck.sh`'s no-op mutation control | **could not fire on Windows** (§5) | fires; verified in both directions |

No assertion was weakened anywhere, and no parser was widened. Both array-store
fixtures still run the same 63 and 108 checks, still assert messages COLD-ONLY,
still use `ITERS = 3000`, and still publish the iteration at which each answer
moved. The mutation controls in §1.3 show both still go red.

---

## 1. `RArrayStoreTiers` and `RArrayStoreInterfaces` — broken oracles five and six

Both are in `CORE_CLASSES`. Both exit 0 and print
`PASS <Class> (N checks)`, so `run.sh` scored them **PASS** and the only guard
that fired was G1. This is not §1 of W8-E9-1's shape — the banner is right, the
count is right, the vector is logically correct — and it is worse in the way
that matters, because the discarded lines are the *entire measurement*:

```
  | RArrayStoreTiers ITERS=3000  (C1 threshold 500; run this both with and without --nojit)
  | s01 String[] as Object[] <- Integer            cold=[ArrayStoreException] hot=[ArrayStoreException]
  … 15 more rows …
```

17 lines for `RArrayStoreTiers` and 29 for `RArrayStoreInterfaces` (measured,
`grep -a . | grep -avE '^(PASS|CK) '`). These two fixtures are the instruments
for a heap-type-confusion defect — an `Integer` landing in a `String[]` once the
storing method tiers up — and the per-row `cold=`/`hot=` pair *is* the evidence
for it. The harness saw one constant line.

### 1.1 The vacuity, measured rather than asserted

A repaired reporter has to be shown to have been broken, so the claim above was
run as an experiment. Model a VM that no longer refuses `String[] <- Integer`
**and whose fixture table agrees with it**, i.e. a VM on which the vector
passes: mutate `s01()` to a legal store and set `EXPECTED_KIND[1]` /
`EXPECTED_COLD[1]` to `no-throw`. On the **pre-repair** fixture:

```
old-mutant rc=0
  old: mutant PASSES? 1  extract lines=1
  old mut.key: [PASS RArrayStoreTiers (63 checks)]
  old: >>> extract() BYTE-IDENTICAL to a correct VM (c51e05f265f56508acd6be0522363b96) —
       the harness CANNOT see the divergence
  old raw diff (what the harness threw away): 2 line(s)
```

The check count is 63 either way, so **G3 could not see it either.** On the
**post-repair** fixture the same mutant:

```
new-mutant rc=0, PASSES, extract lines=35
  new: extract() differs from a correct VM; the cross-VM diff catches it:
      < CK RArrayStoreTiers s01 …  coldmsg=[java.lang.ArrayStoreException:java.lang.Integer]
      > CK RArrayStoreTiers s01 …  coldmsg=[no-throw]
      < CK RArrayStoreTiers s01 …  cold=[ArrayStoreException] hot=[ArrayStoreException] moved=-1
      > CK RArrayStoreTiers s01 …  cold=[no-throw] hot=[no-throw] moved=-1
```

That is the W7-51/W7-60 shape on a **scheduled, green, `CORE_CLASSES`** vector,
and it is the whole argument for this repair.

### 1.2 The repair, following the four siblings rather than inventing a dialect

* one evidence funnel per fixture, `ck(row, fields)`, printing
  `CK <Class> <row> <field>=<value>` **unconditionally** and printing the VM's
  **own** answer — so the two VMs compare their answers *to each other*, not
  each to this file's expectation table.
* two lines per row: `coldmsg=[…]` from pass 1 and
  `cold=[…] hot=[…] moved=<i>` from pass 2.
* `moved` is published on **every** row, not only on a transition. The old code
  appended `MOVED@1375` conditionally; an absent field is not evidence, and the
  iteration index is exactly what a tier-dependent store check shows up as.
  `ITERS` is published for the same reason — read together they are what says
  the compiled tier was reached at all.
  *Checked before publishing it:* `moved` is `-1` on every row on the oracle and
  `md5sum`-stable over three runs, so it introduces no cross-VM flake. A VM on
  which it is **not** `-1` has by definition changed its answer between tiers —
  the `TIER-SPLIT` check already fails there and the run is red on rc alone. A
  jitter in *which* iteration it moved at can therefore only make an
  already-failing run differ from itself, never a passing one.
* `DIVERGENCE <text>` → `CK <Class> FAILED <text>`, printed at the point of
  failure, immediately after the values it is about. On a red run the harness
  could previously see *that* something failed and not *what*.
* the tail is `CK <Class> fails=N`, then `CK <Class> checks=N`, then
  `PASS <Class> (N checks)` **on the clean path only**; `fails != 0` still
  throws before the banner. Separate lines, for W8-E9-1 §1's reason —
  `harness_check_count` does `sub(/^.*checks=/, ""); print`, so a combined line
  publishes a non-numeric "count".

Two properties these fixtures deliberately have were preserved, and one was
made stricter:

* **messages stay COLD-ONLY.** HotSpot's `-XX:+OmitStackTraceInFastThrow`
  (default on) swaps in a preallocated message-less instance once an implicit
  exception is thrown often enough from a compiled site, so a message is not a
  tier-invariant and asserting it hot measures the oracle's optimizer. Pass 1
  runs one execution per site before anything has tiered.
* **`ITERS` stays 3000.** The C1 invocation threshold is 500 and crossing it
  only *enqueues* a background compile, so a fixture stopping at 600 can finish
  before compiled code is entered and read green on a broken VM.
* **rows whose message is not asserted are not published either.** `s14`'s
  helpful-NPE text names a local slot, and `RArrayStoreInterfaces`'s `s12` names
  a generated proxy whose number is not stable. Both were already skipped by the
  assertion; publishing them anyway would have smuggled them into the cross-VM
  diff as assertions nobody adjudicated. This is the one place where "print
  everything" is the wrong instinct.

`RArrayStoreInterfaces` additionally had its **balance line** rescued —

```
illegal stores refused: 12/12   legal stores admitted: 15/15
```

— which is the single most important line it prints (a predicate degenerated to
"allow everything" satisfies every legal row, and the legal rows are the
majority) and which was on a deleted prefix. It is now two `CK` lines, and the
**denominators are counted off `EXPECTED_KIND`** instead of being the hand-typed
literals `12` and `15`: adding a row to the table used to make the ratio quietly
wrong rather than the run red.

**Measured, after** (`RArrayStoreTiers`, abridged):

```
CK RArrayStoreTiers ITERS=3000
CK RArrayStoreTiers s01 String[] as Object[] <- Integer            coldmsg=[java.lang.ArrayStoreException:java.lang.Integer]
…
CK RArrayStoreTiers s01 String[] as Object[] <- Integer            cold=[ArrayStoreException] hot=[ArrayStoreException] moved=-1
…
CK RArrayStoreTiers fails=0
CK RArrayStoreTiers checks=63
PASS RArrayStoreTiers (63 checks)
```

| | raw lines | through `extract()` | dropped | count parses |
|---|---|---|---|---|
| `RArrayStoreTiers` | 35 | 35 | **0** | 63 |
| `RArrayStoreInterfaces` | 59 | 59 | **0** | 108 |

`md5sum`-identical over three consecutive runs each; all guards silent; both
`PASS` in their scheduled positions through the real `run.sh` (§6).

Neither check count changed (63 and 108 before and after) — this repair added
**printing**, not assertions.

### 1.3 Mutation controls

A repaired reporter that can no longer fail is the defect one level up. Both
fixtures, mutated to model the defect they exist for (`s01`'s illegal store made
legal, expectation table left alone):

```
### RArrayStoreTiers MUTANT rc=1
   CK RArrayStoreTiers s01 …  coldmsg=[no-throw]
   CK RArrayStoreTiers FAILED s01 … COLD-MESSAGE: want=[java.lang.ArrayStoreException:java.lang.Integer] got=[no-throw]
   CK RArrayStoreTiers s01 …  cold=[no-throw] hot=[no-throw] moved=-1
   CK RArrayStoreTiers FAILED s01 … COLD: want=[ArrayStoreException] got=[no-throw]
   CK RArrayStoreTiers FAILED s01 … HOT:  want=[ArrayStoreException] got=[no-throw]
   CK RArrayStoreTiers fails=3
   PASS banner present? 0
   extract() diff vs pristine: 10 differing line(s)

### RArrayStoreInterfaces MUTANT rc=1
   … same shape …
   CK RArrayStoreInterfaces illegalRefused=11/12
   CK RArrayStoreInterfaces fails=3
   PASS banner present? 0
   extract() diff vs pristine: 12 differing line(s)
```

The value lines still print, so the diff sees the disagreement even on a VM that
swallowed the throw; no banner is emitted; rc is non-zero; and the balance line
moved.

## 2. `RSslNullSession` — the combined-count line, split

`src/RSslNullSession.java:189` was

```java
System.out.println("CK RSslNullSession checks=" + checks + " failures=" + failures);
```

Measured against the real `harness_check_count`:

```
$ printf 'CK RSslNullSession checks=47 failures=0\n' | harness_check_count RSslNullSession
47 failures=0
```

The `CK` arm matches first and `exit`s, so the parenthesised `PASS` count on the
next line is never reached; G3 then evaluates `[ "47 failures=0" -eq 0 ]`, which
is a **syntax error swallowed by its own `2>/dev/null`**, and the guard neither
passes nor fires. Split into `CK RSslNullSession failures=0` and
`CK RSslNullSession checks=47`, `failures` first so the last word of the counted
line is the count.

**It is ready to schedule.** Measured on the oracle: rc=0, 50 raw lines, 50
through `extract()`, **zero dropped**, `md5sum`-identical over three runs, all
guards silent, and green through the real `run.sh`:

```
  RSslNullSession PASS
  COUNTS: 1 of 1 SCHEDULED vectors passed; 0 scheduled vectors failed; 0 list/coverage errors …; 0 harness-blindness flags …
```

It needs no `class_args` / `class_cv_args` / `class_cp_extra` hook, and it opens
no socket — every arm uses an object that was never connected. Registration is a
word in `run.sh`; see NOM-2.

## 3. One definition of the launch hooks, and a ratchet for the transition

W8-E15-1's NOM-1: `harness-selfcheck.sh` reads the class lists out of `run.sh`
(so a vector scheduled there is a vector guarded here) but launched every vector
with a bare `-cp "$WORK/cls"` and one hard-coded `[ "$c" = RJdkModule ]` line,
while `run.sh` had grown `class_args()` and `class_cp_extra()`. The first vector
that needed one, `RServiceLoaderDoubleSource`, was correctly wired in the suite
and flagged **G4+G3** in the selfcheck. **A guard that reports a defect the tree
does not have is on its way to being ignored**, so the literal two-line patch
was declined in favour of that record's own better suggestion.

`class_args`, `class_cp_extra` and `class_cv_args` now live in
`harness-guard.sh`, next to `extract()`, for the reason that file's header
already gives for `extract()`: the filter the suite diffs through and the filter
the guards reason about drifting apart is the same defect one level up. The same
is true of the configuration each script *launches* under.

**`class_cv_args` moved too even though `harness-selfcheck.sh` never calls it**
(that script runs HotSpot alone). Splitting the family by "who happens to call
it today" is how the next hook gets added to only one of the two files.

`CPSEP` is defaulted in `harness-guard.sh` only when the caller has none, so
`run.sh`'s own `uname`-derived value — computed before it sources the file —
still wins and the two cannot disagree.

### 3.1 `run.sh` is not this lane's file, so the migration is half-landed on purpose

`run.sh` sources `harness-guard.sh` at line 375 and defines its own copies at
line 474+. **Later definition wins**, so `run.sh`'s behaviour is unchanged
bit-for-bit; `harness-selfcheck.sh` defines none and therefore uses the shared
ones. Deleting `run.sh`'s copies is NOM-3.

Leaving two copies in the tree without a guard would be the drift this change
exists to remove, so `harness_hooks_drift()` (**H1**) extracts `run.sh`'s copies
and compares the two definitions **by behaviour**: every hook × every listed
class × a four-point environment matrix (`HAVE_MODULE` set/unset ×
`CRATONVM_ARGS` with/without `--jdk-only`, because `class_args` and
`class_cp_extra` branch on the first and `class_cv_args` on the second). No
command substitution in the loop — the hooks `printf` to stdout and the dump
inherits it; `$(...)` per class would be ~1,100 forks on Git Bash.

**This is how "both callers still behave" was checked**, and it is a stronger
statement than reading the two files:

```
=== H1-M0: pristine run.sh (3 copies present, must agree) ===
  SILENT (bad=0)
  matrix rows compared: 1140 (= 3 hooks x 94+1 classes x 4 environments)
  hooks extracted from run.sh: 3
```

1,140 answers, identical on both sides — so which definition wins cannot matter,
which is exactly what makes the migration safe to land in two pieces.

Three-way control:

| # | state | result |
|---|---|---|
| H1-M0 | pristine tree | **silent** |
| H1-M1 | one arm added to `run.sh`'s `class_args` only | **fires**, naming the first differing answer in all four environments |
| H1-M2 | `run.sh`'s three copies deleted (NOM-3 applied) | **silent** — nothing left to guard, and the guard did not have to be deleted to stop firing |

H1-M2's `run.sh` is `bash -n` clean, so NOM-3 is a pure deletion.

## 4. G6 — the reporting dialect, written down and made self-enforcing

**Seven fixtures** have now been found reporting in a spelling the harness
deletes or cannot parse (`RShutdownHooks`, `RSimpleTimeZoneRaw`,
`RJdkStringCodePoints`, `RFsSingleton`, and the three in this record). That is
not seven careless authors. It is a contract that lived in a `grep` expression,
an `awk` snippet and three records — nowhere a fixture author would look.

Two things landed. First, **the contract is stated** at the top of
`harness-guard.sh`: what an evidence line, a count line, a failures line and a
banner must look like, and the five spellings that are one character away and
silently wrong. Second, **G6 enforces it**, in two arms.

### 4.1 The standing judgement, addressed rather than overturned

W8-E9-1 NOM-4 recommended **against** widening `harness_check_count` to accept
`PASS <Class> checks=N`: 76 of 77 counting vectors already use the parenthesised
form, and widening a parser so a slip stops being visible is the wrong
direction. **That judgement is upheld and G6 goes the other way** — it makes the
near-miss loud. The near-misses are rare (1 in 77 for each spelling) and
enumerable, which is precisely the profile that makes a lint worth having and a
widened parser not.

### 4.2 Two arms, because the two halves are visible to different things

G6 adds **no new entry point**: it is folded into `harness_guard_oracle` and
`harness_guard_extract`, which `run.sh` and `harness-selfcheck.sh` already call.
Nothing in `run.sh` had to change, and G6 findings join the existing
harness-flag population, so `COUNTS:` still has four terms and still closes.

**Deleted-prefix arm** (`harness_dialect_nearmiss`, inside G1 and G4). G1 already
fires on any dropped line, so this arm adds no point — it upgrades the
*diagnosis*. Every pattern names a fixture that actually used it; it is a
measured list, like `HARNESS_NOISE_RE`, not a defensive one. It also fires
inside **G4's early return**, which is the case W8-E9-1 §2 got bitten by: G4
returns before G1 runs, so `RSimpleTimeZoneRaw`'s `RESULT … PASS` produced a
"silent vector" message and fixing only the banner would have swapped one guard
for another.

**Kept-prefix arm** (inside `harness_guard_extract`). These spellings *survive*
`extract()`, so G1 is blind to them by construction and this is the only thing
that can fire:

* **N1** `PASS <Class> checks=N` — the count reads as absent, so the remedy
  looks like "add a row to `harness-uncounted.txt`" rather than "add two
  characters". It hid a published count for a year (`RShutdownHooks`).
* **N2** `CK <Class> checks=N <anything else>` — the worst of the set, because
  it fails **inside the guard** rather than in the vector: G3 silently no-ops.
* **N3** two published counts that disagree — the only thing that can notice a
  banner literal that stopped tracking its counter.

### 4.3 Mutation matrix — 8 mutants, 8 fires, pristine silent

Driven through the real `harness-selfcheck.sh` with `MUTATE=`:

| # | mutation | guard | diagnosis |
|---|---|---|---|
| M0 | pristine | — | **silent**, `1 sound, 0 flagged` |
| M1 | `PASS <C> checks=63` | **G6/N1** | "UNPARENTHESISED spelling, which harness_check_count's PASS arm does not accept" |
| M2 | `CK <C> checks=63 fails=0` | **G6/N2** | "the count line carries more than the count … G3 SILENTLY NO-OPS" |
| M3 | banner literal `(99 checks)` vs `checks=63` | **G6/N3** | "publishes TWO check counts and they disagree" |
| M4 | `RESULT <C> PASS (63 checks)` | **G4+G6** | "banner INVERSION … fixing only the banner would leave the G1 below" |
| M5 | evidence funnel to a bare prefix (31 lines) | **G1+G6** | "carries a `key=value` observable on a deleted prefix" |
| M6 | `ok ITERS=3000` | **G1+G6** | "the `ok <label>=<value>` dialect" |
| M7 | `@@RESULT checks=63` | **G1+G6** | "@@RESULT is a DELETED prefix — write the count as: CK <C> checks=N" |
| M8 | `pass <C> (63 checks)` | **G4+G6** | "CASE — the filter is case-SENSITIVE" |

One case G6 deliberately does **not** reach, stated so it is not quoted for
more: a `DIVERGENCE`-prefixed failure line on a **red** oracle. `harness_guard_oracle`
returns at G4 on `rc != 0`, by design — everything G1 says about a sick oracle's
output is uninteresting — so the near-miss classifier never sees it. The
repaired fixtures print `CK <Class> FAILED …` instead, which is the fix rather
than the detection.

### 4.4 The eighth fixture, found on the first run

Run over all 94 scheduled vectors on the oracle, G6 is silent on 93 and fires on
one that no census had reached:

```
  HARNESS ERROR [G6] RJdkProcess: the count line carries more than the count, so
    harness_check_count returns a non-numeric string and G3 SILENTLY NO-OPS:
      | CK RJdkProcess checks=55 skipped=[]
---------------------------------------------
HARNESS SELF-CHECK: 93 vectors sound, 1 flagged (RJdkProcess)
```

`src/RJdkProcess.java:417`. Its own header, at line 57, says a wrong count "on
the `checks=` line … is strictly worse than a missing check" and cites CratonVM
printing 51 against HotSpot's 53 — so this is a vector whose author cared
specifically about the number the harness cannot currently read. One line, NOM-1.

**G6 was landed hot rather than ratcheted, and that is a judgement.** A waiver
file for a guard whose whole purpose is to make a near-miss visible would
reproduce the defect one level up — a gate that measures a fraction and reads as
good news. The blast radius is bounded and was measured: `RJdkProcess` is in
`JDKONLY_CLASSES` and **not** in `CORE_CLASSES`, so the default `bash
regression-suite/run.sh` (`SUITE=core`) is unaffected; only `SUITE=jdk-only`,
`SUITE=all` and `JDK_ONLY=1` gain the flag, and the remedy in NOM-1 is two
lines.

## 5. The mutation control that could not fire — found in this lane's own file

`harness-selfcheck.sh` refuses a `MUTATE=` expression that changed nothing, and
its comment says why: *"A mutation that changed nothing would produce a green run
that looks like evidence and is not. This is the whole failure mode the file is
about."* The test was `cmp -s "$WORK/src/$c.java" "$HERE/src/$c.java"`.

**It could not fire on Windows.** `sed -i` on Git Bash rewrites the file with LF
endings whatever the expression did:

```
after cp:            IDENTICAL
after no-op sed -i:  DIFFERS
sizes: orig=16534 after=16190      # exactly one byte per line, 344 lines
```

So `cmp` differed for every mutant, and `MUTATE='RFoo:s/typo_that_matches_nothing/x/'`
reported a clean run as a mutation control — the exact failure mode the check
was written to prevent, in the check itself. Found because M9 of §4.3's matrix
was that no-op and the script accepted it.

Now compared as content, normalised for exactly that rewrite, against the
pre-`sed` state rather than the source tree. Verified in both directions: the
no-op is refused with `rc=3`, and M1 still runs and still fires.

## 6. What the suite reports now

`CORE_CLASSES` 57, `JDKONLY_CLASSES` 37, `SUITE=all` 94 — unchanged; this lane
registered nothing.

Measured against the real `run.sh` with a HotSpot stand-in for `$CV`, in an
isolated scratch repo:

```
SUITE=core       57 passed, 0 failed
  COUNTS: 57 of 57 SCHEDULED vectors passed; 0 scheduled vectors failed; 0 list/coverage errors (never scheduled); 0 harness-blindness flags (per-vector flags, not extra vectors).

SUITE=jdk-only   37 passed, 1 failed ( failed: harness:RJdkProcess )
  COUNTS: 37 of 37 SCHEDULED vectors passed; 0 scheduled vectors failed; 0 list/coverage errors (never scheduled); 1 harness-blindness flags (per-vector flags, not extra vectors).
```

Both decompositions close: `P + V = scheduled` (57+0, 37+0) and
`F = V + L + H` (0 = 0+0+0, 1 = 0+0+1).

**`SUITE=core` is the line this lane changes**, and the change is the removal of
a flag, not the addition of one: W8-E15-1 §4 measured it as
`57 passed, 2 failed ( failed: harness:RArrayStoreTiers harness:RArrayStoreInterfaces )`
with `2 harness-blindness flags`. It is now **`57 passed, 0 failed`** with all
four terms zero.

The one remaining flag is NOM-1's `RJdkProcess`, and once that lands
`SUITE=jdk-only` and `SUITE=all` read all-zeros too. Two `COVERAGE WARNING`s
remain (`RSslNullSession`, NOM-2, and `RJdkBridge1`, NOM-4); both become
`COVERAGE ERROR` under `STRICT_COVERAGE=1`, **which this file's own
documentation says CI should set**.

---

## 7. Nominations

### NOM-1 (BLOCKING for `SUITE=jdk-only`/`all`) — `RJdkProcess`'s combined count line

`regression-suite/src/RJdkProcess.java:417`. Replace

```java
        System.out.println("CK RJdkProcess checks=" + checks + " skipped=" + skipped);
```

with

```java
        System.out.println("CK RJdkProcess skipped=" + skipped);
        System.out.println("CK RJdkProcess checks=" + checks);
```

Both lines survive `extract()`, so no evidence is lost and the `skipped` list
keeps its own line. Measured consequence today: `harness_check_count RJdkProcess`
returns `55 skipped=[]` and G3 no-ops, i.e. this vector's count has never been
compared. Owner: whoever owns that file. It should land in the same wave as this
record.

### NOM-2 — register `RSslNullSession`

`run.sh` is not this lane's file. The registration is one word; per §2 the
vector is ready. Add to `CORE_CLASSES`:

```
… RSimpleDateFormatZone RJdkIntrinsics3 RSslNullSession"
```

`CORE_CLASSES` 57 → 58, `SUITE=all` 94 → 95, and one `COVERAGE WARNING` goes
away. No `class_args` / `class_cv_args` / `class_cp_extra` hook is needed. It is
a JSSE-contract vector asserting `--real-jdk` COMPATIBILITY behaviour with no
network, which is why `CORE_CLASSES` and not `JDKONLY_CLASSES` — the same
reasoning `run.sh` already records for `RJdkViews`. It is predicted **RED on
CratonVM** until E12-1's fabricated-cipher fallbacks are fixed; that is the gate
doing its job, not a bad registration.

One caveat measured but not owned here: two of its expectations —
`getPacketBufferSize=16709` and `getEnabledProtocols=[TLSv1.3, TLSv1.2]` — are
properties of the **JDK's** JSSE configuration, not of the JSSE contract, and
they were measured on Temurin/Microsoft 25.0.3+9. Both VMs are launched with the
same `$JDK`, so the cross-VM diff is unaffected; but a different oracle JDK or a
site `java.security` could move them, and that would look like a VM defect. That
is the vector author's design decision, restated so the next reader of a red row
checks the oracle first.

### NOM-3 — delete `run.sh`'s three hook copies

`class_args()`, `class_cp_extra()` and `class_cv_args()` in `run.sh` (lines
474–590) are now duplicated in `harness-guard.sh`, which `run.sh` sources
BEFORE them. Deleting the three function bodies is a pure deletion — the shared
ones take over unchanged, the resulting file is `bash -n` clean, and H1 goes
silent on its own (verified, §3.1 H1-M2). `run.sh`'s own `CPSEP` block may stay
or go; `harness-guard.sh` defaults it only when unset, so either way there is
one value. Until this lands, H1 holds the two copies equal on every selfcheck
run.

### NOM-4 — `RJdkBridge1` is registered nowhere

`regression-suite/src/RJdkBridge1.java` is in no class list and in no
`UNREGISTERED_CLASSES` row — the `RJdkPhaser` shape again, `COVERAGE WARNING` on
every run and fatal under `STRICT_COVERAGE=1`. Not examined by this lane beyond
noticing it; it needs a list entry or an `UNREGISTERED_CLASSES` row with a
reason. Owner: whoever owns that file.

### NOM-5 — index this record

`docs/known-issues/jdk-only/INDEX.md` is not this lane's file; add a row next to
W8-E15-1's.

---

## 8. The lesson

**A fixture that passes its own assertions and publishes an honest count can
still be reporting nothing.** `RArrayStoreTiers` had a correct banner, a correct
count, 63 real assertions and a comment explaining its own tier model — and a
VM that stopped refusing an illegal array store produced an extract byte-identical
to a correct one. Every guard that reasons about *whether* a vector reported was
satisfied. The one that mattered is the guard that asks *how much of what it
measured survived the filter*.

**And the sharper one: seven authors did not each make the same mistake — one
contract was unwritten.** The four earlier repairs each fixed a fixture; this one
had to fix the fixtures *and* write the contract down *and* make it enforce
itself, because the population of near-misses was still growing. G6 found the
eighth on its first run over the corpus, which is the evidence that hand-census
was never going to be the mechanism. The thing to resist when the eighth arrives
is the temptation to widen the parser: a spelling the tooling quietly accepts is
a spelling nobody ever fixes.
