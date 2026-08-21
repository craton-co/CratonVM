# W2 — the collection object model past `HashMap`

`HashMap` is done and verified against the oracle. You are doing the same for
the families behind it, in the order the evidence supports.

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

## 4. WHAT IS ALREADY DONE, and what it cost to learn

`HashMap`'s internal representation now matches HotSpot **byte-for-byte in the
default configuration**:

```
CratonVM r10, --jdk-only, unarmed        HotSpot 25.0.3+9
map1 [Ljava.util.HashMap$Node; real=3    map1 [Ljava.util.HashMap$Node; real=3
map2 [Ljava.util.HashMap$Node; real=3    map2 [Ljava.util.HashMap$Node; real=3
```

That took **two halves that both had to land**:

* **the node class** (`H16`) — `AnonymousObject$4` IS the `HashMap.Node`;
* **the array component type** (`H23` + H0) — the table was
  `[Ljava.lang.Object;`.

**They fail in opposite directions**, which is why half-fixed was not fixed: a
fabricated node in a typed array throws `ArrayStoreException`; a real node in an
`Object[]` stores fine but misleads everything that reads the component type.
Node-class-first was the safe order; the reverse would have produced a real
`Node[]` full of fabrications.

**And the fix landed inert once before it worked.** `H23` typed
`native_map_init_capacity`, `map_resize` and `lhm_init_with_cap` — but the
**no-arg `HashMap()` constructor** still called `alloc_ref_array`, which
hard-codes the sentinel. Three arms were green with the fix doing nothing.
**Enumerate every allocation site on the path you are changing, including the
ones reached through helpers.**

## 5. YOUR TASK, in evidence order

1. **`Hashtable`.** `H23` DECLINED it, measured: its nodes are `HashMap$Node`
   where HotSpot has `Hashtable$Entry`, and
   `Hashtable$Entry.isAssignableFrom(HashMap$Node)` is **false on both VMs** —
   typing that table without fixing the nodes arms exactly the hybrid. **The
   node-half defect `H16` closed for `HashMap` is still open one family over.**
   `H17` also found unarmed `Hashtable` is built by `HashMap`'s minter: length
   16 with `HashMap$Node` heads, against HotSpot's 11 and `Hashtable$Entry`.
2. **The view carriers.** `KeySet`/`EntrySet`/`Values` are minted with `this$0`
   **deliberately null**, contained by registrar rows — containment that arming
   removes. `H4-1`: the split must be **per carrier family, keyed on whether the
   PRODUCING registrar moved**, and `MAP_VIEW_CARRIERS` spans four families, so
   retagging that one registrar is a partial retag of three of them.
3. **`TreeMap`.** `H0-5` §6 measured that mechanism A (null `this$0`) reaches
   `LinkedHashMap` and `Hashtable` but **NOT `TreeMap`** — so `TreeMap` does not
   ride along on that repair and needs its own diagnosis.
4. **`ConcurrentHashMap`.** `H23` declined it too: `chm_publish_real_table`
   already types the real table, and its node class is a *different* class
   (`ConcurrentHashMap$Node`).

**`IdentityHashMap` is CORRECT with `Object[]` — on HotSpot too.** Not a defect.
Do not "fix" it.

## 6. A REGION H0 HOLDS BRIEFLY

H0 is landing a guard on `register_comparator_natives` in your file, to close
the last standing `SUITE=all` failure. It lands before you start; `git pull` the
branch and you will have it. **Everything else in the file is yours.**

## 7. Acceptance

`105/105`, `104/105` (or better — see trap 6), `65/65`. **Verify with a
reflective probe against HotSpot, not with the arms** — the corpus asks nothing
about node classes or component types, and would have passed a completely inert
fix. Predict what moves; name your falsifier.
