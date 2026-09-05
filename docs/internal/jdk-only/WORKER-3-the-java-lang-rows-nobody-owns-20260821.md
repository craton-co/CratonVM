# WORKER 3 — the `java.lang` and `java.lang.invoke` rows no plan points at

The largest unowned block in the defect population.

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

## 4. YOUR TASK

`H14-2` measured that **445 rows (31.7%) are claimed by no P0/P1/P2 row at
all**. Your share is the two biggest blocks:

| block | rows |
|---|---:|
| `java.lang` core | **168** |
| `java.lang.invoke` | 56 |
| `StringBuilder` / `StringBuffer` | 57 — **see the warning below** |

`H25` already adjudicated **all 214 owned registrations** in `lang_class.rs`,
`lang_string.rs` and `deprecated_lang.rs`, retired **zero**, and produced **six
refusals with evidence**. Read `H25-1`, `H25-2`, `H25-3` before doing anything —
your job starts where that adjudication ends, and several of its refusals are
blocked on files you now own.

### The strongest thing it found — R1, and it is a live wrong answer

**`Thread.stop()V` is double-registered.** The winner throws
`UnsupportedOperationException`, matching JDK 25 bytecode. **The loser, at
`lib.rs:14080`, silently interrupts the thread and returns.** Deleting the
winner promotes the loser and scores as a census win. This is trap 4 in a new
file, and it is the sharpest example of it anyone has found.

### A fourth verb, beyond `H14-1`'s three

`H25-1`: **342 registrations name a method no JDK 25 image declares.** 314 own
their slot, all at 0 invocations against a positive control of 49 non-zero rows
in the same dump. 70 are `java/lang`. `javap -p` over all 102 receiver classes
splits them **286 truly gone / 56 near-miss**, where the class declares the
method *name* but no overload with the registered descriptor —
`Unsafe.park(Object,long)` vs `park(boolean,long)`, `Paths.get(String)` vs
`get(String,String...)`. Interceptions somebody intended that have **never once
executed**.

**They are invisible to the census by construction, so retiring them predicts a
delta of ZERO — which is a PASS, not a failure.**

**But 342 is a ONE-IMAGE upper bound**, and `H25` corrected itself on this:
`lang_string.rs:12422` carries a 56-line comment headed *"On JDK 25 this
registration never fires, and that is not a defect"* — `StringUTF16.isBigEndian`
is kept deliberately for JDK 17/21 images. **WORKER 5 owns the multi-image sweep;
coordinate with it before deleting anything in this class.**

### ⚠ `StringBuilder` — do not retire, and know why

`H14-3` priced `register_string_builder_natives` at **zero vectors**. `H22`
measured what that zero conceals: armed across the three classes its registrar
covers, **every `append` is silently discarded and `toString()` returns empty —
no exception, `rc=0`.** H0 reproduced and isolated it: `StringBuilder` alone is
fine, `AbstractStringBuilder` alone is fine, **together they break.**

**It is 192 of the 214 registrations in `lang_string.rs` — 90% of your write
surface.** (An earlier brief told a lane it was in someone else's file. It was
not. That error is recorded in `H25-3`.)

The zero is real and means nothing: **the corpus never asserts the CONTENT of a
built string.**

## 5. Acceptance

`105/105`, `104/105`, `65/65`. Census is **1387** and should fall by roughly the
rows you retire — but `H22` predicted −24 and measured −15, because **the census
counts shadows actually DISPATCHED, not registrations removed.** Predict your
delta and treat a smaller one as expected rather than as failure.
