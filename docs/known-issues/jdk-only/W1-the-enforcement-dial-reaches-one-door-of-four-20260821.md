# W1 — the enforcement dial, and why nothing can be priced until it is fixed

**This is the highest-leverage item in the project.** Every retirement anyone has
costed was costed with an instrument that does not do what the four records
quoting it assume.

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
| **W1** | `vm/src/runtime/interpreter/**`, `vm/src/runtime/env_cache.rs` | the enforcement dial |
| **W2** | `native-collections/src/lib.rs` | collection object model past `HashMap` |
| **W3** | `native-builtins/src/lang_class.rs`, `lang_string.rs`, `lang_invoke.rs`, `deprecated_lang.rs` | the `java.lang` + `java.lang.invoke` unclaimed rows |
| **W4** | `native-io/src/**`, `native-builtins/src/deprecated_io_util.rs`, `native-builtins/src/phases_late/io_streams.rs` | the `java.io` / NIO unclaimed rows |
| **W5** | `regression-suite/probes/**`, `scripts/**`, `docs/` | multi-image triage + the instruments |
| **H0** | `regression-suite/run.sh`, `regression-suite/harness-guard.sh`, `native-collections/src/lib.rs :: register_comparator_natives` (in flight, lands first) | the last standing failure + harness |

Everyone may create `docs/known-issues/jdk-only/W<n>-*.md`. **Nobody but H0
touches `INDEX.md`** — put your index rows at the end of your own record and H0
will move them.

## 1. THE STATE OF THE WORLD, measured

```text
  CRATONVM_ARGS=--jdk-only   105 / 105
  SUITE=all                  104 / 105   RJdkFunctionCombinators (H0 is on it)
  SUITE=core                  65 /  65

  CENSUS  native-shadows-bytecode 1387  ·  bytecode-won 481
          synthetic-native-registered 1622  ·  compatibility_classes 0
```

Four of five long-standing `SUITE=all` failures closed this week. The **defect
population is 1402 shadows over 149 registrars**, 1402/1402 attributed (`H14-1`).
The published plan (`H0-4`'s six collection prefixes) is **200 rows, 14.3%** of
it, and **445 rows (31.7%) are claimed by no P0/P1/P2 row at all** (`H14-2`).

**Completion is roughly 5% by any denominator that tracks the contract**, against
a gate that reads 100%. That gap is the project.

## 2. NINE TRAPS THAT HAVE ALREADY COST SOMEBODY A DAY

1. **The JDK path recipe in older briefs is poison.** `JDK="$(dirname "$(dirname
   "$(command -v javap)")")"` yields the MSYS POSIX spelling; `run.sh` exports
   `MSYS_NO_PATHCONV=1`, so it reaches `cratonvm.exe` unconverted and the VM dies
   in argument parsing. The harness then prints a bare `cratonvm rc=1`,
   **indistinguishable from a real assertion failure.** MEASURED A/B: POSIX form
   0 passed / 5 failed, Windows form 5 passed / 0 failed. **Use
   `cygpath -m "$(dirname "$(dirname "$(command -v javap)")")"`.**
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
8. **Never run two `regression-suite/run.sh` at once.** `.guard-tmp` is a fixed
   shared path; concurrent sweeps starved the oracle and moved a cell from
   83/104 to 102/104.
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

`CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>` is meant to make contract §1.4
enforced rather than counted — the native stops winning, real JDK bytecode runs,
exactly as a permanent retirement would. **It does not.**

`H17-2` MEASURED, and H0 verified by grep, that
`jdk_only_enforce_shadow_for` has **exactly one live call site**:

```
env_cache.rs:752                      definition
native_override.rs:2499               doc comment
native_override.rs:7433               THE ONLY LIVE CALL   <- inside resolve_step1_native
```

So **arming a class arms only that class's cold, step-1 dispatches.** Warm
invoke-cache entries, the force-native interceptor, reflective `Method.invoke`
and JIT binds never ask.

### What that invalidated

`H0-4`'s six-family table (`HashMap` 81/104 and the rest), `H0-3`'s CHM eleven,
`H14-3`'s **thirteen** arms including the five "free" registrars and
`Properties` at 65/104, and every `H15`/`H22` armed measurement. All of them
price a **hybrid** state no retirement can reach: `H16-3` photographed a real
`Node[]` holding one real node and two fabrications.

**The direction of the error is not knowable** — a hybrid can be worse than
uniform-fabricated or better. What holds is the asymmetry: **failures are real,
zeros are unreliable.**

### Deliverables

1. **Teach the other dispatch doors to consult the dial.** `H17-3` carries the
   full specification, the two hazards, and an **instrumented-build-first** step
   (four per-door counters) that turns every ARGUED claim in it into a number.
   Do that step first.
2. **Do not repeat the 2026-08-04 accident.** This file already paid for this
   once: a `java/lang/String` arm was deleted because *"a method's behaviour
   started depending on how many times its call site had run."* Wiring a door
   naively reintroduces exactly that.
3. **Fix the witnesses, or say which survive.** `H17` measured that **four of
   six witnesses are blind on a current binary** — bucket head class,
   `modCount`, `hashCode()` counts, `equals()` counts. One went blind **because
   `H16` fixed the VM**. Only the `table` array class still discriminates, and
   `H23`+H0 have now typed that too, so it may be blind by the time you read
   this. **You may need to build a new witness before you can measure anything.**
4. The census is a **deduplicated presence set with no counts**, and under
   `enforce` it records only the bytecode-won half. Say whether that should
   change.

### Acceptance

Unarmed arms must **not move**: `105/105`, `104/105`, `65/65`. This is a dial
that is off by default — if the default configuration shifts, your change
reached further than the dial.

**Armed numbers are EXPECTED to get worse, and that is the point.** They will
finally be the real price. Predict the direction and name your falsifier.
`H17-1` predicts they go **down**, falsified if a fixed dial leaves `HashMap` at
or above 81/104 — which would mean cold step-1 dispatches were already the
overwhelming majority and `H0-4`'s table can stand.
