# `RJdkEnumerations` is RED on `dev` — bisected to the ConcurrentHashMap values-cursor change

**Status: OPEN, BISECTED, NOT THIS BRANCH'S.** 2026-08-27. No fix applied; the
owning lane's call, with a verified revert available.

## 1. The failure

```text
CRATONVM_ARGS=--jdk-only  regression-suite   111 passed, 1 failed
SUITE=all                                    111 passed, 1 failed
   failed: RJdkEnumerations

java.lang.AssertionError: ConcurrentHashMap.elements(): hasMoreElements() never terminated
  at RJdkEnumerations.drain(RJdkEnumerations.java:113)
  at RJdkEnumerations.concurrentHashMap(RJdkEnumerations.java:208)
```

`drain` caps the enumeration at 96 elements for a map with far fewer, so the
enumeration is not terminating. **`m.keys()` on the same map, three lines
earlier, PASSES** — it is the VALUES enumeration specifically.

## 2. CORRECTED — the flag was the VECTOR's, not the defect's

**This section first read "it is only visible under `--jdk-only-report`". That
was wrong, and wrong in the direction that makes a bug look narrower than it
is.** Corrected the same day, from a probe written for something else.

What was true: the suite adds `--jdk-only-report` to every vector under
`--jdk-only`, and my first bare invocation of `RJdkEnumerations` passed 3/3
while the suite's failed 3/3. A vector that passes by hand and fails in the
suite *is* a difference in the invocation, and that reasoning was sound.

What was wrong: I stopped at the first invocation difference that reproduced,
and concluded the DEFECT needed the flag. `probes/Phase1Sweep.java` — written to
re-adjudicate the roadmap's P1-F lane, not to chase this — drains a
three-element `ConcurrentHashMap` and reports:

```text
P1-F chm keys      [a, b, c]           <- fine, both modes
P1-F chm elements  NEVER TERMINATED    <- compatible AND --jdk-only
```

with **no `--jdk-only-report`, no suite, and no flags at all**. Three `put`s and
an `elements()` drain are enough. So the report flag changes only whether
`RJdkEnumerations`' particular shape trips it; the values-cursor defect is
plainly reachable without it, in both modes.

**The lesson is about a reproduction, not about this bug.** Finding *an*
invocation difference that flips the result is not the same as finding *the*
condition the defect needs, and the first one you hit is the one you are most
likely to over-claim. The bisect in §3 was unaffected — it was already run
against the real cause — but the scope statement here would have sent the owning
lane looking at the report path.

## 3. Bisected, in two builds

| tree | result |
| --- | --- |
| `85a91ece5` (this branch's tip) | FAIL 2/2 |
| minus this branch's three tail fixes | **FAIL 2/2** — so not this branch's |
| minus `a0168ed03` only, tail fixes still in | **PASS 3/3** |

`a0168ed03` *fix(collections): ConcurrentHashMap's values view gets its own
cursor, and the iterator yield stays out*, merged as `a52eaefc8`
(`perf/itr-bytecode-yield-20260827`).

The commit names the exact subsystem the symptom points at — the values view —
and reverting it alone, with everything else in place, clears the vector. The
first revert (of my own three fixes) is the one that mattered for attribution:
without it the natural assumption is that the newest change broke it.

## 4. What is NOT claimed

* No mechanism. This record bisects; it does not explain why a values cursor
  fails to terminate only when the report sink is active. The report path
  instrumenting dispatch is the obvious place to look, and it is the owning
  lane's code.
* No revert landed. `a0168ed03` carries a measured performance win, and undoing
  another lane's landed work on my own initiative is not this branch's call.
  The revert is **verified to clear the vector** if they want it as a stopgap.
* Not a `--jdk-only` defect in the mode sense: it reproduces in COMPATIBLE mode
  too, with no flags (§2).

## Reproduce

The smallest form, no flags, either mode — three `put`s and a values drain:

```bash
cratonvm --java-home "$JDK" -cp probes/out Phase1Sweep
```

Look for `P1-F chm elements |NEVER TERMINATED|`; `P1-F chm keys` beside it is
fine, which is what points at the values cursor.

The original suite form, for the bisect record:

```bash
cratonvm --java-home "$JDK" --jdk-only --jdk-only-report /tmp/rep -cp regression-suite/build RJdkEnumerations
```
