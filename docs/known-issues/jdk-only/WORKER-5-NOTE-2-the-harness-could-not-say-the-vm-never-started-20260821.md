# WORKER-5 NOTE 2 — the harness could not say "the VM never started", and the discriminator was already in the VM's own output

**Status: FIXED, MEASURED.** Lane WORKER-5, 2026-08-21, against
`C:/craton/cratonvm-r10.exe`. Verified end to end on both arms of the fault.

> **Read §1.1 first.** H0 landed `dab993033` for the same defect while this
> lane was working, and it **CORRECTED the diagnosis every earlier account of
> trap 1 gave, including this record's first draft**. The two fixes are
> complementary and both are in the tree; §7.1 says how they compose.

---

## 1. The blindness

`run.sh`'s `run_pass()` reduces a non-zero CratonVM exit to

```bash
state=FAIL; why="cratonvm rc=$cvrc"
sig=$(… | grep -aiE 'AssertionError|NoSuchMethod|linkage error|panic|SEGV|fatal' …)
[ -n "$sig" ] && why="rc=$cvrc: $sig"
```

**Nothing in that alternation can match a VM that died before it reached the
vector.** There is no assertion, no linkage error and no panic — the process
refused its own command line, or could not find the class, or was killed by
`timeout`. Every one of those printed the bare `cratonvm rc=1` that a genuine
assertion failure prints.

That is trap 1 of the WORKER briefs and it cost a lane a day. The JDK-path
recipe in the older briefs,

```bash
JDK="$(dirname "$(dirname "$(command -v javap)")")"
```

yields the MSYS POSIX spelling; `run.sh` exports `MSYS_NO_PATHCONV=1` (line 60,
deliberately, so the path reaches `cratonvm.exe` intact); so the POSIX path
reaches `cratonvm.exe` **unconverted** and the VM exits before running anything.
**Every vector in the run goes red for a reason the harness cannot name.**

### 1.1 CORRECTION — it is not an argument-parsing failure, and three of us said it was

`H24-2` said the VM "dies in argument parsing". H0 repeated it. **The first
draft of this record repeated it, and so did the first version of the
classifier's own output label.**

It is wrong. The VM **accepts** the POSIX spelling on the command line and dies
**validating the JDK image**:

```
Provide a valid JDK installation (must contain `jmods/` or `lib/modules`).
```

The reason three readers converged on the same error is that **the VM's own
banner says so**: `jdk mode: <not yet resolved — failure occurred during
argument parsing>`. That banner is a reliable KEY — it is emitted exactly when
the VM exits before resolving its mode, which is the thing worth detecting — and
an unreliable DESCRIPTION. This classifier greps it and does not quote it.

> **FIXED AT THE SOURCE, 2026-08-21 (H0).** The banner no longer lies. Reaching
> `None` there means only that `ACTIVE_JDK_MODE` -- a `OnceLock` set when the
> class library is selected -- was never set, and there are TWO ways to do that:
> clap refusing the command line (rc=2, and clap prints its own `error:` line
> anyway), or the JDK image failing validation at `vm-cli/src/main.rs:3844`
> (rc=1, the common one). The arm asserted the first and was reached by the
> second. It now reads `<not yet resolved — the failure occurred before the
> class library was selected>`, which is true of both.
>
> **This record's design survived the change, and that is the point.** Matching
> on the PREFIX `jdk mode: <not yet resolved` rather than on the sentence meant
> a fix to the sentence could not break the classifier -- it still passes, and
> the truncated `— x` fixture was already asserting exactly that property.
> The three selftest fixtures that quoted the old sentence in full were
> refreshed to the new one so they keep describing a VM that exists; a fixture
> that outlives the output it imitates is a quiet lie even when the test is
> green.
>
> Independently measured, three invocations of `cratonvm-r11`: a POSIX spelling
> of a REAL jdk, and a path that does not exist at all, BOTH return **rc=0** for
> `-version` and print `JDK class library root: NONE FOUND`. Argument parsing
> accepted both. See `fix(diag): the mode line named a phase it cannot observe`.

Category 1 is therefore two classes now:

| output also carries | class |
|---|---|
| `Provide a valid JDK installation` / `--java-home path does not exist` | `VM REJECTED THE JDK IMAGE` |
| neither | `VM EXITED BEFORE RESOLVING ITS JDK MODE (launch/config)` |

Both hints say in as many words that this is **not** a command-line parse
failure, so the corrected diagnosis is carried where the next reader will be.

## 2. What the VM actually prints — MEASURED, `cratonvm-r10.exe`

The discriminator was already there and nothing read it.

| fault | rc | the line that names it |
|---|---:|---|
| bad `--java-home` | 1 | `jdk mode: <not yet resolved …>` **plus** `Provide a valid JDK installation …` (the second line is the real cause — §1.1) |
| unknown flag | 2 | `error: unexpected argument '--no-such-flag' found` + `Usage: cratonvm-r10.exe …` |
| flag missing a value | 2 | `error: a value is required for '--java-home <PATH>' but none was supplied` |
| conflicting modes | 2 | `error: the argument '--jdk-only' cannot be used with '--synthetic-jdk'` |
| **missing main class** | **1** | `Could not find or load main class NoSuchMain` **and** `[cratonvm] jdk mode: real-jdk (java.home=…)` |

**The last row is the control that makes the first a signal.** A VM that got far
enough to resolve its JDK mode PRINTS the mode; a VM that did not prints
`<not yet resolved`. So "the VM never got as far as a vector" is a one-line
grep, and it is the line the harness never looked at.

The premise is pinned rather than assumed: the selftest asserts that the
EXISTING alternation still finds nothing in the trap-1 output, and goes red if a
future VM starts printing `fatal` there.

## 3. The fix

`regression-suite/harness-vmfault.sh` — a new file with `vm_fault_class` and
`vm_fault_hint`, and all the tests. It classifies eight faults:

* **the JDK image rejected** — the trap-1 shape (§1.1),
* any other exit before the JDK mode was resolved,
* command-line rejection (rc=2 **and** `^error: ` **and** `^Usage: `/`try '--help'` — all three, because rc=2 alone and `error:` alone are each reachable by a vector),
* missing main class,
* `rc=124` — the harness's own `timeout`, not a failure. **Trap 7: `RMapGcStress` needs 233 s against a 120 s budget; use `TIMEOUT=600`.**
* `rc=126`/`127` — `$CV` is not an executable. Nothing ran.

and returns 1 for everything else, so `run.sh` falls through to its existing
grep unchanged.

`run.sh` and `harness-guard.sh` are H0's. The change to `run.sh` is three small
hunks — the `.` beside the `harness-guard.sh` source, an `if` around the existing
`sig` grep, and one summary block — reproduced in §6 below. **Reverting the
commit and deleting the new file restores the previous behaviour exactly**;
nothing else in the suite depends on it.

### 3.1 Two design constraints that are not obvious

* **ONE LINE per classification.** The fault this exists for fails ALL 105
  vectors, so a four-line explanation would print 105 times. The long form is
  `vm_fault_hint` and the summary prints it **once**.
* **It adds NOTHING to `$total_fail`.** Every vector it names is already counted
  red by `$fail`. A second point would be exactly the double-count the G2/G3
  note in `run.sh` was written to avoid. It changes the WORDS, never the
  NUMBERS.

## 4. Verified end to end, both arms

`ONLY="RArraysMismatch RAtomicArray"`, same binary, same JDK, only the spelling
of `JDK=` differing:

```text
POSIX --java-home     0 passed, 2 failed
  RArraysMismatch FAIL  rc=1: HARNESS FAULT — VM REJECTED THE JDK IMAGE:
                        --java-home path does not exist or is not a directory:
                        /c/Program Files/Microsoft/jdk-25.0 [on MSYS use cygpath -m]
  …
  ENVIRONMENT: 2 vector(s) had no VM to answer them — the run below is not a
  measurement of CratonVM. Last seen: …
    (the six-line hint, ONCE, saying explicitly that this is not a parse failure)

cygpath -m form       2 passed, 0 failed        <- control, output unchanged
```

(The label read `VM COULD NOT PARSE ITS ARGUMENTS` when first measured; the run
above is the re-verification after §1.1's correction. The classification, the
counts and the control arm are unchanged — only the sentence is.)

The classifier was also driven against the **real binary**, not only against
transcribed strings: four live runs (POSIX home, unknown flag, missing main
class, clean run) classify correctly, and the clean run falls through.

## 5. Every failure path fires, and so does every negative control

`bash regression-suite/harness-vmfault.sh --selftest`:

```text
  ok   bad --java-home / unknown flag / flag missing a value / missing main class
  ok   startup failure that is not the image
  ok   timeout / not +x / no binary
  ok   real AssertionError                          (falls through)
  ok   vector exits 2 with its own error: line      (falls through)
  ok   vector printing the word Usage               (falls through)
  ok   clean pass                                   (falls through)
  ok   SIGSEGV                                      (falls through)
  ok   every classification is exactly one line
  ok   vm_fault_hint covers all six classes and refuses anything else
  ok   run.sh's existing sig grep is still blind to the trap-1 output (the premise)
```

**The five negative controls matter more than the seven positives.** A
classifier that fired on a real assertion failure would relabel the very
failures the suite exists to report — it would be a worse defect than the one it
fixes. The `vector exits 2 with its own error: line` case is why the
command-line check requires all three signals rather than the obvious two.

## 6. The hunks in `run.sh`, for review

```diff
 . "$HERE/harness-guard.sh" || { echo "ERROR: cannot source …"; exit 3; }
 harness_load_uncounted "$HERE/harness-uncounted.txt"
+
+# The ENVIRONMENT-fault classifier. …
+. "$HERE/harness-vmfault.sh" || { echo "ERROR: cannot source …"; exit 3; }
```

```diff
     if [ "$cvrc" -ne 0 ]; then
       state=FAIL; why="cratonvm rc=$cvrc"
+      if envwhy=$(vm_fault_class "$cvrc" "$cvout"); then
+        why="$envwhy"
+        ENV_FAULT_N=$((ENV_FAULT_N+1)); ENV_FAULT_LAST="$envwhy"
+      else
       sig=$(printf '%s\n' "$cvout" | grep -aiE 'AssertionError|…' …)
       [ -n "$sig" ] && why="rc=$cvrc: $sig"
+      fi
```

```diff
+if [ "$ENV_FAULT_N" -gt 0 ]; then
+  echo "  ENVIRONMENT: $ENV_FAULT_N vector(s) had no VM to answer them — …"
+  echo "    $ENV_FAULT_LAST"
+  vm_fault_hint "$ENV_FAULT_LAST"
+fi
 # ---- the instrument's own verdict ---------------------------------------
```

## 7. What this does NOT establish

* **It does not detect a VM that started and then died for an environment
  reason** — a missing DLL, an exhausted heap at init, a `--java-home` that
  exists but is not a JDK past the directory check. Those still reach the
  `sig` grep. The five classified shapes are the ones MEASURED on this binary,
  not a proof that the set is closed.
* **The rc=2 rule is heuristic**, not a contract with the VM. It requires three
  co-occurring signals precisely because rc=2 is not reserved.
* **Nothing was measured about the ORACLE side.** A HotSpot run that dies in
  argument parsing is `harness_guard_oracle`'s territory (G1/G4) and was not
  touched.
* **This changes reporting, not verdicts.** No vector that failed now passes,
  and no count moved. If a run's numbers change after this lands, that is a bug
  in this change.

## 7.1 How this composes with H0's `dab993033`

H0 fixed the same blindness from the other end, and the merge keeps both:

* **`vm_fault_class` runs FIRST** and NAMES the fault. It is the only half that
  can see the three faults which produce **no output at all** — rc=124 (with the
  `TIMEOUT=600` remedy), rc=126/127 — because a fall-through over stdout has
  nothing to fall through to.
* **H0's chain runs in the `else`** and guarantees `why` is never bare for
  anything this classifier declines: clap's `^error:`/`^Usage:` lines, then the
  VM's own first non-noise line tagged `unclassified:`, then `no output`.

Neither subsumes the other: mine is a named-fault table (precise, closed), H0's
is a fall-through (imprecise, exhaustive). **The merged region was read line by
line rather than trusted to the clean auto-merge** — nothing is lost and nothing
fires twice.

H0's is also the fix that produced the evidence for §2.1: its `unclassified:`
tag surfaced `Provide a valid JDK installation …`, which is how the
argument-parsing story was falsified at all.

## 8. NOMINATIONS

* **N1 — delete the poison recipe wherever it is still written down.** The
  measured A/B is POSIX 0 passed/5 failed vs Windows 5 passed/0 failed. The
  correct line is
  `JDK=$(cygpath -m "$(dirname "$(dirname "$(command -v javap)")")")`.
* **N2 — `harness-selfcheck.sh` should run this selftest.** It already
  reproduces G1–G4 without a CratonVM build; `vm_fault_class` needs neither a
  build nor a JDK, so it belongs in the same place. Not done here: that file is
  H0's.
* **N3 — an `ENVIRONMENT:` line should probably be FATAL to the run's exit
  status even when every vector "only" failed.** Today the run is red anyway
  because the vectors are red, so nothing is lost; but a run in which the VM
  never started should not be able to look like a run in which it lost. Left
  alone deliberately — it is a change to the exit-status contract, and that is
  H0's decision, not this lane's.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-2` — `run.sh`'s `sig` grep cannot match an argument error, so a
  broken `--java-home` reddens every vector as a bare `cratonvm rc=1`. FIXED by
  `regression-suite/harness-vmfault.sh` + three hunks in `run.sh`. Seven faults
  classified, five negative controls, the premise pinned. MEASURED both arms.
