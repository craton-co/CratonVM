# WORKER-5 NOTE 5 — one case per process turns `H0-8`'s retraction into a disproof, and `size()` makes the map WORSE

**Status: MEASURED.** Lane WORKER-5, 2026-08-21, against
`C:/craton/cratonvm-r10.exe`, oracle HotSpot 25.0.3+9. No VM change. Answers
`H0-8` N4.

> **Re-run on `cratonvm-r11.exe` after merging H0's `dab993033`** (which brings
> `ecd4f56e1`, a `java/util/Comparator` guard in `native-collections`). All six
> cases reproduce **exactly**, in both arms and against the oracle, and the run
> is again `ok — every case gave one stable answer per arm across 3 rotated
> rounds`. `withsize` still gives **0**.

---

## 1. What was retracted, and what "retracted" left open

`H13-1` reported a third `ConcurrentHashMap` mechanism with five findings.
`H0-8` measured that discriminators 2–5 were **artifacts of the order the cases
ran in** — swap two cases and the failure follows the POSITION — and retracted
them. `H17-2` then named the cause: `jdk_only_enforce_shadow_for` reaches only
COLD, step-1 dispatches, so the first use of a call site is dialled and later
uses hit the warm cache.

A retraction says *"this was not shown"*. It does not say what is true.
`H0-8` N4 asked for the probe to be split per process or annotated so the next
reader could not re-derive the error. Splitting it also answers the open
question.

## 2. The instrument

`ChmConsistencyProbe.java` now takes **one case** and refuses two, with the
retraction in its class comment. `chm-consistency.sh` drives all six cases in
their own VMs and repeats the set `ROUNDS` times **rotated by one**, so a result
that still depends on position appears as a case whose answer moved between
rounds and is reported `ORDER-DEPENDENT` rather than printed as an answer.

Two defences rather than one, because either alone can be argued with: fresh
process means no case can be "the second one"; rotation means a residual
position effect is still caught if it can be.

## 3. MEASURED — 3 rotated rounds, every case in its own process

**Every case gave ONE STABLE ANSWER per arm across all three rounds.** The
confound is gone, and with it the ambiguity:

| case | unarmed | **ARMED** | oracle | `H13-1` claimed | verdict |
|---|---|---|---|---|---|
| `plain` | size=4 ✓ | **size=1 keys=[]** | size=4 | the symptom | **CONFIRMED** |
| `withsize` | size=4 ✓ | **size=0 keys=[]** | size=4 | `size()` FIXES it | **DISPROVED** |
| `withabs` | size=4 ✓ | **size=1 keys=[]** | size=4 | `Math.abs` FIXES it | **DISPROVED** |
| `intkeys` | size=4 ✓ | **size=1 keys=[]** | size=4 | `Integer` keys are fine | **DISPROVED** |
| `viachm` | ck=true ✓ | ck=true ✓ | ✓ | the two doors | **DISPROVED** |
| `viamap` | ck=true ✓ | ck=true ✓ | ✓ | disagree | **DISPROVED** |

* **All four retracted discriminators are now positively FALSE**, not merely
  unproven. Every put-shaped case fails identically under arming, and neither
  read door disagrees with the other at all.
* **NEW, and in no record: `withsize` gives `size=0`.** Interposing `size()`
  between the puts does not fix the map — it is **strictly worse than the plain
  case's 1**. `H13-1` reported that interposition as the fix; de-confounded, it
  loses the last surviving entry too.
* **The unarmed arm is correct in all six** and matches the oracle exactly, so
  nothing here says the shipping default is affected. That is consistent with
  `H0-8` §4.
* The primary symptom stands: `size()==1` with an empty `keySet()` under arming
  is a real divergence, and `H13-1` §4's traced consequence stands with it —
  `CryptoPermissions.isEmpty()` true for a map whose `size()` is 1, so
  `JceSecurity.<clinit>` throws and the JCE is dead for the process.

## 4. The other half of probe hygiene: nothing compiles the probes

Probes are not in the suite's `CLASSES` list, so nothing ever compiles them and
a broken one is found by the next person who needs it. **Four probes filed in
one week did not compile**, all because the public class name did not match the
file name; they were fixed by hand.

`regression-suite/probes/check-probes.sh` found a **FIFTH still broken at
`22cb4338d`**: `Sweep5CollectionContracts.java` declaring `public class Sweep5`,
which `javac` refuses outright (*"class Sweep5 is public, should be declared in
a file named Sweep5.java"*). Hand-fixing was not converging. Fixed here; **all
23 probes in the directory now compile standalone.**

It compiles one file at a time on purpose — a batch compile lets one probe
resolve another's class, and every probe is run as its own `-cp <dir> <Class>`.

Both failure paths were exercised against the real tree, not argued:

```
restore the Sweep5 shape   -> 1 name mismatch(es)   rc=1
add an uncompilable probe  -> 1 compile failure(s)  rc=1
restore                    -> 0 / 0                 rc=0, 23 probes
```

## 5. `--selftest` for both, no VM and no JDK

`chm-consistency.sh --selftest` proves `rotate()` actually rotates — a rotation
that returned its input would make every round identical and the order check
vacuously green, and the first draft had exactly that bug (`set -- $1` clobbered
`$2`) — then replays **`H0-8`'s own retracted numbers as data** and fails if the
order check stops catching them.

`check-probes.sh --selftest` drives six declaration shapes (`class`,
`final class`, `enum`, `record`, `interface`, none), the exact `Sweep5` shape,
and a commented-out declaration that must not be read as code.

## 6. What this does NOT establish

* **It does not explain the mechanism.** It measures that six cases now give
  stable answers and that four claimed discriminators are false. WHY an armed
  CHM reports 1 (or 0) is `H0-5`/`H17-2` territory and is untouched here.
* **`withsize` giving 0 is measured, not diagnosed.** It is worse than `plain`
  and repeatably so across three rounds. Nothing here says why interposing
  `size()` costs an entry.
* **n=4, not n=3000.** `H0-4` §7's numbers are at 3000 entries. These cases use
  four. The shapes agree; the magnitudes were not compared.
* **Six cases, one class, one scope.** `java/util/concurrent/ConcurrentHashMap`
  only. `H0-8` N1 asked for **every armed measurement in the directory** to be
  re-run one case per process — `H0-4`'s table, `H0-3`'s eleven, `H14-3`'s
  thirteen arms, `H22`'s screen. That is not done, and it is a larger job than
  this record.
* **`check-probes.sh` does not RUN anything.** A probe that compiles can still
  be confounded or blind; those are this script's two neighbours.

## 7. NOMINATIONS

* **N1 — `H0-8` N1 is still open** and is now cheaper to act on: the
  one-case-per-process + rotation pattern in `chm-consistency.sh` is ~40 lines
  and transfers to any multi-case probe.
* **N2 — `withsize` → 0 wants an owner** (§3). It is a strictly worse result
  than the case the whole CHM thread has been quoting, and it was invisible
  while the cases shared a process.
* **N3 — `check-probes.sh` belongs in CI, or at least in
  `harness-selfcheck.sh`.** It needs a JDK and no build, and it runs in seconds.
  Five broken probes in one week is the argument.
* **N4 — the `H13-1` record should carry a pointer to this table.** It still
  reads as though `size()` and `Math.abs` fix the map. `H0-8` retracted that;
  this disproves it, and the two facts live in different files.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-5` — `ChmConsistencyProbe` split one-case-per-process
  (`H0-8` N4 answered) + `chm-consistency.sh` with rotated repeats. **All four
  of `H13-1`'s retracted discriminators are now positively DISPROVED**, not
  merely unproven; the unarmed arm is correct in all six cases; and
  **`puts+size()` under arming gives `size=0`, worse than the plain case's 1**
  — new. Also `check-probes.sh`: a FIFTH probe
  (`Sweep5CollectionContracts.java`) did not compile; all 23 now do. MEASURED.
