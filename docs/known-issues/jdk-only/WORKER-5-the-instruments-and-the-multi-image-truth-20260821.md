# WORKER 5 — the instruments, and the multi-image question nobody has asked

Everyone else's numbers depend on your work. Half the "findings" this week were
instrument artifacts.

> **Naming note.** These are `WORKER-n-*`, NOT `W<n>-*`. `W1`–`W8` is an
> EXISTING namespace in this directory (`W2-1`, `W7-84`, `W8-E6-1`, … — the
> historic wave records). The first draft of these files used it and would
> have filed `W2-the-collection-object-model` directly beside
> `W2-1-strict-refuses-the-synthetic-stream-stack.md`. Do not reuse it.

## 0. WHERE EVERYONE IS WORKING — read before you touch anything

| | worktree | branch |
|---|---|---|
| **H0 (orchestrator)** | `C:\craton\cratonvm\.claude\worktrees\h2-known-issues-206dee` | `claude/jdk-only-mode-handoff-09b48c` |
| **you** | your own worktree, cut from that branch | same branch |

**H0 builds and runs the arms. You do not build.** H0 is the only party that
merges. Cut your worktree from the branch tip and `git merge --ff-only` before
your first edit — **eleven of eleven lanes so far have started 100+ commits
behind** and one of them nearly re-derived a record already in its gap.

### Ownership — disjoint by construction, do not cross

| worker | owns | subject |
|---|---|---|
| **WORKER 1** | `vm/src/runtime/interpreter/**`, `vm/src/runtime/env_cache.rs` | the enforcement dial |
| **WORKER 2** | `native-collections/src/lib.rs` | collection object model past `HashMap` |
| **WORKER 3** | `native-builtins/src/lang_class.rs`, `lang_string.rs`, `lang_invoke.rs`, `deprecated_lang.rs` | the `java.lang` + `java.lang.invoke` unclaimed rows |
| **WORKER 4** | `native-io/src/**`, `native-builtins/src/deprecated_io_util.rs`, `native-builtins/src/phases_late/io_streams.rs` | the `java.io` / NIO unclaimed rows |
| **WORKER 5** | `regression-suite/probes/**`, `scripts/**`, `docs/` | multi-image triage + the instruments |
| **H0** | `regression-suite/run.sh`, `regression-suite/harness-guard.sh`, `native-collections/src/lib.rs :: register_comparator_natives` (in flight, lands first) | the last standing failure + harness |

Everyone may create `docs/known-issues/jdk-only/WORKER-<n>-NOTE-*.md`. **Nobody but H0
touches `INDEX.md`** — put your index rows at the end of your own record and H0
will move them.

## 1. THE STATE OF THE WORLD, measured

```text
  CRATONVM_ARGS=--jdk-only   105 / 105
  SUITE=all                  105 / 105   ALL GREEN as of 2026-08-21
  SUITE=core                  65 /  65

  CENSUS  native-shadows-bytecode 1387  ·  bytecode-won 481
          synthetic-native-registered 1610  ·  compatibility_classes 0
```

**UPDATE, 2026-08-21: the corpus is GREEN IN EVERY ARM for the first time.** The
fifth and last long-standing `SUITE=all` failure, `RJdkFunctionCombinators`,
closed when H0 guarded the `java/util/Comparator` family on a real image
(`ecd4f56e1`). `synthetic-native-registered` fell **1622 → 1610**, the twelve
registrations no longer made — which is the narrowest available confirmation
that the guard took effect rather than landing inert.

**This raises the bar for you rather than lowering it.** Until today "verdict
neutral" meant *do not grow the failing set*. There is no failing set now:
**any red vector you produce is yours**, and a green arm is no longer weak
evidence that nothing broke — it is the only evidence, and it is still evidence
about the question it asked (trap 5). The corpus still does not check array
component types, the CONTENT of a built string, or the class identity of a
returned object.

**And do not read the green as progress against the contract.** The defect
population is unchanged at **1387 shadows**; §1 below still stands. All five
closures were COMPATIBLE-mode defects that strict mode already fixed.

Four of five long-standing `SUITE=all` failures closed this week. The **defect
population is 1402 shadows over 149 registrars**, 1402/1402 attributed (`H14-1`).
The published plan (`H0-4`'s six collection prefixes) is **200 rows, 14.3%** of
it, and **445 rows (31.7%) are claimed by no P0/P1/P2 row at all** (`H14-2`).

**Completion is roughly 5% by any denominator that tracks the contract**, against
a gate that reads 100%. That gap is the project.

## 2. NINE TRAPS THAT HAVE ALREADY COST SOMEBODY A DAY

1. **The JDK path recipe in older briefs is poison.** `JDK="$(dirname "$(dirname
   "$(command -v javap)")")"` yields the MSYS POSIX spelling; `run.sh` exports
   `MSYS_NO_PATHCONV=1`, so it reaches `cratonvm.exe` unconverted. **Use
   `cygpath -m "$(dirname "$(dirname "$(command -v javap)")")"`.** MEASURED A/B,
   same binary, only the spelling changed: POSIX 0 passed / 5 failed, Windows
   5 passed / 0 failed.

   **CORRECTION 2026-08-21: it is NOT "the VM dies in argument parsing"** — both
   `H24-2` and H0 said that and both were wrong. The VM accepts the POSIX
   spelling on the command line; it dies **validating the JDK image**:
   `Provide a valid JDK installation (must contain 'jmods/' or 'lib/modules')`.
   Nobody could see that because the harness printed a bare `cratonvm rc=1`.

   **That half is now FIXED.** `run.sh`'s signature extraction falls through to
   clap's `^error: `/`^Usage: ` lines and then to the VM's own first line, tagged
   `unclassified:`, with `no output` when the VM printed nothing. A launch or
   config failure no longer renders identically to an assertion failure. The fix
   diagnosed this very bug the first time it ran.
> **CORRECTION 2026-08-22 — the one-call-site premise below is FIXED.**
> `jdk_only_enforce_shadow_for` now has a call site at every one of the
> fourteen dispatch doors, and the leak that premise describes is gone
> (MEASURED before the fix: 890 of 947 armed `Bridge` dispatches never
> asked the dial). **The direction stated here is also wrong**: armed
> cells taken with the one-door dial were not a floor — `java/util/HashMap`
> scored 81/104 half-armed and 86/104 fully armed, because half-armed is a
> corrupt hybrid, not a partial retirement. See
> `WORKER-1-the-dial-now-reaches-every-door-20260821.md`.

2. **The enforcement dial reaches ONE dispatch door.**
   `jdk_only_enforce_shadow_for` has exactly one live call site, inside
   `resolve_step1_native`. Arming a class arms only its **cold, step-1**
   dispatches — warm invoke-cache, the force-native interceptor, reflective
   `Method.invoke` and JIT binds are outside it by construction. **An armed
   FAILURE is real. An armed ZERO is unreliable.**
3. **Multi-case probes in one process are confounded.** Anything latched per
   process is confounded with case order. Four "discriminators" of a supposed
   third CHM defect were pure position (`H0-8`). **One case per process, or
   randomise and repeat.**
4. **162 triples are registered more than once** and only the `owns_slot: true`
   one is reachable. Retiring the winner **promotes the loser**. `H22` nearly
   put 16 already-condemned bodies into service while scoring a 24-row win.
   Check `--dump-native-registry` before and after every deletion.
5. **A green arm is evidence about the question it asked.** The corpus asks
   nothing about array component types, the CONTENT of a built string, or the
   class identity of a returned object. `H23`'s table fix was green three arms
   running and completely inert; a nine-line reflective probe caught it.
6. **These failures are COMPATIBLE-mode defects.** All five standing failures
   pass under `--jdk-only` and fail only without the flag (`H15-1`). **A vector
   going GREEN is the fix, not a violation.** Four have closed that way.
7. **`RMapGcStress` is a TIMEOUT**, `rc=124`: it needs 233 s against a 120 s
   budget. Not an objection to your change. **Use `TIMEOUT=600`.**
8. **~~Never run two `regression-suite/run.sh` at once.~~ FIXED 2026-08-21 —
   you may now sweep concurrently.** `.guard-tmp` was a FIXED shared path and its
   `rm -rf` deleted a running sweep's oracle files underneath it. It is now
   PID-scoped (`$HERE/.guard-tmp.$$`) with a `trap` cleanup, and two concurrent
   two-vector sweeps were MEASURED green with no harness errors and no leftover
   directories.

   **Keep the SIGNATURE, because it will recur elsewhere.** It corrupted a
   measurement twice and both times looked like a VM defect first: `H14-3` §5
   moved a cell from **83/104 to 102/104**, and H0 got **3 passed, 204 failed**
   — every vector red AND a `harness:` entry for each, `204 = 102 vectors x 2`.
   **Total redness that INCLUDES the harness guard is an ENVIRONMENT failure, not
   a defect.** No source change can fail `RJitGc`, `RCrypto` and `RShutdownHooks`
   in one run. If you ever see that shape, check what else is running before you
   revert anything.
9. **Never patch source with heredoc-python (`python - <<'PY'`).** It bakes
   literal control characters in; the file still parses and is silently wrong.
   Write a `.py` to a scratch dir and run it. Verify with `cat -A`.

## 3. HOUSE RULES

* **Commit small and often.** Six lanes died to infrastructure faults this week;
  one held 950 uncommitted lines and came within a tool call of losing them.
* **Mark every claim `MEASURED` (you ran it) or `ARGUED` (you read it).** Do not
  blur them. `H15-3`'s "eleven stand-ins" was ARGUED from a grep; `H24` measured
  it and **ten of the eleven were dead**, making it one fix rather than five.
* **A refusal with evidence beats a retirement without it.** `H22`'s best result
  was refusing two of five "free" retirements — one of which silently empties
  every `StringBuilder`.
* **Grep before asserting the tree does not know.** Repeatedly, the thing a lane
  "discovered" was already documented as deliberate.
* Predict your outcome and **name what would falsify you** before you measure.

---

## 4. WHY THIS LANE EXISTS

A partial list of this week's measurement failures, all MEASURED:

* the enforcement dial reaches **one of four+ dispatch doors** (trap 2);
* **four of six witnesses are blind** on a current binary, one of them *because
  the VM was fixed* — `H16` taught the native to mint real `HashMap$Node`s, so
  the bucket-head witness now agrees in both directions;
* a supposed third CHM defect was **four order artifacts** of a multi-case probe
  (`H0-8`);
* the fabrication ratchet was wrong **six times**: 84 → 89 → *rc=0 while
  matching nothing* → three invented carrier families → 29 of 147 arrays → 359;
* the observation sink **capped at 256** and announced saturation through a
  boolean nothing read, so every count published before 2026-08-20 is a FLOOR;
* `run.sh`'s `477 bytecode-won` is **453 distinct triples**, 24 double-counted;
* `CRATONVM_NATIVE_SHADOW_SINK_CAP=200000` is **silently discarded** (ceiling
  65,536, no warning);
* the `jit_compile` sub-sink reports `truncated: null`, which `run.sh`'s
  saturation grep can never match.

## 5. YOUR TASK

### 5a. The multi-image sweep — this one changes other people's answers

`H25-1` found **342 registrations naming methods no JDK 25 image declares**, then
corrected itself: `StringUTF16.isBigEndian` is kept **deliberately** for JDK
17/21 images, with a 56-line comment saying so. **So 342 is a ONE-IMAGE UPPER
BOUND and this host has only JDK 25.**

**Get JDK 17 and 21 images and re-run the classification.** Until then, W3 and
W4 cannot safely delete anything in that population — a row that is dead on 25
may be load-bearing on 21. This is the single most unblocking thing you can do
for the other lanes.

### 5b. Fix the harness's argument-error blindness

Trap 1: a bad JDK path makes the VM die in argument parsing, and the harness
prints a bare `cratonvm rc=1` — **indistinguishable from a real assertion
failure.** The `sig` grep has **no pattern for "the VM could not parse its
arguments"**. Add one. `H0` owns `run.sh` and `harness-guard.sh` — **send the
patch, do not apply it.**

### 5c. The ratchet's structural limit

`scripts/untyped-alloc-ratchet.sh` v6 counts **direct spellings, not reach**.
`alloc_ref_array` is ONE counted site with **159 callers**; routing a caller to
a typed allocation removes a real fabrication and moves the number by zero —
measured 2026-08-21. Either give it call-graph awareness or state the limit
where people read the number, not only in the file header.

### 5d. Witnesses

Build a witness that survives the VM getting more correct. Every existing one
was a coincidence of the VM being wrong in a visible way. **WORKER 1 may be unable to
measure its own fix without this.**

### 5e. Probe hygiene

`regression-suite/probes/ChmConsistencyProbe.java` is **confounded as filed**
(trap 3). `ChmOrderConfound{OrderTest,KeyOrder,DoorOrder}.java` are the
order-swapped controls. Either split the first per process or annotate it —
leaving it invites the next reader to re-derive a retracted finding.

Four probes filed earlier this week **did not compile** — the public class name
did not match the filename. All fixed; check any you add.

## 6. Acceptance

You mostly do not change VM behaviour, so the arms should not move: `105/105`,
`104/105`, `65/65`. **Where you do change a gate, exercise every failure path
and say so** — a gate that cannot fail is worse than no gate, and this project
shipped five of those in one file.
