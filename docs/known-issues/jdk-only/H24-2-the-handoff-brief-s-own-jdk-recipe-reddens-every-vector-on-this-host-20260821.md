# H24-2 — the handoff brief's own `JDK=` recipe reddens every vector on this host, and the failure is indistinguishable from a VM defect in the summary line

**Status: MEASURED AND UNDERSTOOD. No source change — the defect is in an
instruction, not in the tree.** Lane H24, 2026-08-21, on the prebuilt
`C:/craton/cratonvm-r8.exe` (clean build at `025780ff7`). Oracle HotSpot
**25.0.3+9** at `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`.

Every claim is marked **MEASURED** (this lane ran it today) or **ARGUED**
(this lane read it).

This record exists because it cost this lane an hour and would have cost it the
whole session: the recipe produces a run in which **the five vectors that are
supposed to be green are red**, with a verdict line that reads exactly like a
VM regression. Three of those five were closed this week. A lane that took the
summary at face value would have reported the closures reverted.

---

## 1. The recipe, and what it produces

The wave-H handoff brief states the oracle resolution as:

```
JDK="$(dirname "$(dirname "$(command -v javap)")")"
```

**MEASURED**, on this host, that assigns:

```
/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot
```

— the **MSYS POSIX** spelling, because `command -v` answers in MSYS's own
namespace. The correct value for this harness is the Windows spelling
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`.

## 2. What goes wrong, MEASURED

`regression-suite/run.sh` exports two variables at its top (line 60):

```bash
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
```

Those disable MSYS's automatic POSIX-to-Windows argument rewriting **for every
argument of every child process**, which the harness needs so that class-path
strings and `-D` values survive intact. `cratonvm.exe` is a native Windows
binary. It is then handed `--java-home /c/Program Files/...`, a path that does
not exist in the Win32 namespace, and dies during argument parsing:

```
[cratonvm] main-vm run() returned Err: --java-home path does not exist or is
not a directory: /c/Program Files/Microsoft/jdk-25.0.3.9-hotspot
Provide a valid JDK installation (must contain `jmods/` or `lib/modules`).
[cratonvm] jdk mode: <not yet resolved — failure occurred during argument parsing>
```

**This is the whole mechanism.** It is not intermittent, not load-dependent,
and not vector-specific: it hits every vector in the run, because
`--java-home "$JDK"` is on the per-vector command line unconditionally
(`run.sh:692`).

## 3. Why the summary line hides it — the part that makes this a trap

The harness classifies a non-zero rc and then tries to name the cause by
grepping CratonVM's output for a signature (`run.sh:701-704`):

```bash
sig=$(printf '%s\n' "$cvout" | grep -aiE 'AssertionError|NoSuchMethod|linkage error|panic|SEGV|fatal' ...)
[ -n "$sig" ] && why="rc=$cvrc: $sig"
```

The argument-parsing failure matches **none** of those six patterns, so `$sig`
is empty and `why` falls back to the bare `cratonvm rc=1`. The printed verdict
is therefore:

```
  RJdkModule     FAIL  cratonvm rc=1
```

which is the same line a genuine assertion failure would print if its message
happened not to match the grep. The two harness-blindness guards that fire
alongside it (`G2 nothing survives extract()`, `G3 publishes no check count`)
are **also** exactly what a real red vector produces, because a vector that dies
before its banner leaves an empty key by construction — `run.sh:743-756` says so
in its own comment. So all three signals are consistent with "the VM broke", and
none of them mentions the JDK path.

## 4. The A/B, MEASURED

Identical binary, identical worktree, identical `ONLY=` list, back to back. The
**only** change is the spelling of `$JDK`:

| `JDK=` | result |
|---|---|
| `/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot` (the brief's recipe) | **0 passed, 5 failed** — `RJdkModule RJdkServices RJdkProxyIface RJdkEnumerations RImmutableFactoryTypes` |
| `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot` | **5 passed, 0 failed** |

The second row is the expected state and it reproduces the brief's own baseline:
with the two target vectors added, `5 passed, 2 failed`, the two being
`RJdkFunctionCombinators` and `RServiceLoaderDoubleSource`.

**The negative control that proves it is the path and not the VM**: running the
byte-identical harness command line by hand, but from a shell WITHOUT the two
MSYS exports, gives `rc=0` and `PASS RJdkModule (163 checks)`. The VM was never
involved. Reproduced in a scratch script that sets the two exports and nothing
else — it flips `cvrc` from 0 to 1 on its own.

## 5. Why `command -v javap` is nonetheless the right idea

The recipe's intent is sound and worth keeping: derive the oracle from whatever
JDK is actually first on `PATH` rather than hard-coding a path that 66 records
already disagree about (`H5-1` §6.1 records the Adoptium-versus-Microsoft
split). Only the **spelling** is wrong. On an MSYS host the fix is one command:

```bash
JDK="$(cygpath -m "$(dirname "$(dirname "$(command -v javap)")")")"
```

`cygpath -m` yields the mixed form (`C:/Program Files/...`) which **both**
`cratonvm.exe` and MSYS `bash` accept — Windows-native for the child process,
still slash-separated so `"$JDK/bin/javap"` and `[ -x "$HS" ]` keep working
inside the script. `cygpath -w` would give backslashes and break the bash-side
uses.

**NOT VERIFIED:** whether this bites on the Linux build host. **ARGUED** it does
not — `MSYS_NO_PATHCONV` is inert off Windows and `command -v` already answers
in the native namespace there — but this lane ran nothing on Linux.

## 6. What this does NOT say

* It does not say the harness is wrong to set the MSYS exports. `run.sh:60`
  needs them; without them the class-path separator and `-D` values are mangled.
  The defect is the interaction, and the fixable half is the recipe.
* It does not say any previously reported wave-H result is invalid. Lanes that
  used a Windows-form `$JDK` were unaffected. This lane cannot tell from the
  outside which lanes used which spelling, and **did not check** whether any
  landed record contains a red row caused by this.

## 7. NOMINATIONS

* **N1 — put the `cygpath -m` spelling in the handoff brief and in
  `regression-suite/README.md`.** The recipe as written is a green-to-red
  generator on the primary development host, and it is quoted verbatim into
  every wave-H lane brief.
* **N2 — `run.sh` should validate `$JDK` once, before the per-vector loop, and
  fail loudly.** It already fails loudly for an unresolvable `ROOT` (`run.sh:75`)
  for precisely this reason — "measured a DIFFERENT TREE and reported perfectly
  well-formed results about it" is the same species as "measured no VM at all
  and reported 5 failures". A one-line `[ -d "$JDK/jmods" ] || [ -d "$JDK/lib" ]`
  guard converts an hour of misattributed VM debugging into one message.
* **N3 — add the argument-parsing signature to the `sig` grep.** `main-vm run\(\)
  returned Err` would have named the cause on the first line of output. As it
  stands the harness's own diagnosis facility is blind to the entire class of
  "the VM refused its command line", which is the class where the vector name is
  least informative.
* **N4 — a lane brief that hands out a shell recipe should hand out its expected
  output too.** `JDK` echoing `/c/...` versus `C:/...` is a one-glance check that
  no lane performed, this one included, until five green vectors went red.

---

## CONFIRMED INDEPENDENTLY (lane H0, 2026-08-21) — and the recipe is mine

Reproduced on `cratonvm-r8.exe`, one vector, same binary, **only the spelling of
an exported `JDK` changed**:

```
JDK exported in POSIX form      RJdkHello  FAIL  cratonvm rc=1
   ( /c/Program Files/... )                HARNESS ERROR [G2] nothing survives extract()
                                           HARNESS ERROR [G3] publishes no check count

JDK exported in Windows form    RJdkHello  PASS
   ( C:/Program Files/... )                REGRESSION SUITE: 1 passed, 0 failed
```

**This record is right, and the recipe it indicts is one I wrote into every lane
brief this session.** `JDK="$(dirname "$(dirname "$(command -v javap)")")"`
yields the MSYS POSIX spelling; `run.sh` exports `MSYS_NO_PATHCONV=1`, so it
reaches `cratonvm.exe` unconverted and the VM dies in argument parsing. The
harness's `sig` grep matches none of its six patterns and prints a bare
`cratonvm rc=1` — **indistinguishable from a real assertion failure.**

### The near-miss is the point

This lane says it *"nearly reported them reverted"* — three vectors that closed
this week. A lane following my brief, on a host where `JAVA_HOME` is unset,
would have seen the entire corpus red and reported a catastrophic regression
that did not exist.

### Why the published baselines are NOT affected — checked, not assumed

```
JAVA_HOME (this shell) = C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot
run.sh line 83         = JDK="${JDK:-${JAVA_HOME:-C:/Program Files/Java/jdk-25}}"
```

The orchestrator never **exported** `JDK`, so `run.sh` fell through to
`JAVA_HOME`, which is already the Windows spelling. **`105/105`, `103/105` and
`65/65` stand.** Stated with the evidence rather than asserted, because "my
numbers are fine" is exactly the claim that should not be taken on trust after a
finding like this one.

Note also that the failure is **narrower than "the recipe is poison"**: passed
via `JAVA_HOME` the VM normalises *both* spellings and neither fails. It breaks
only when exported as `JDK` and forwarded as an argument. That distinction
matters, because it explains why the recipe survived a dozen lanes before biting.

### The fix, and the wider lesson

`cygpath -m "$(dirname "$(dirname "$(command -v javap)")")"` — verified to
produce `C:/Program Files/...` and to pass.

**A harness that renders a configuration error identically to a test failure
will eventually be believed.** Two `HARNESS ERROR` lines fired here and both
were true, but neither said "your JDK path is unusable" — they said the vector
produced no output, which is downstream. The `sig` grep having no pattern for
"the VM could not parse its arguments" is the actual gap.
