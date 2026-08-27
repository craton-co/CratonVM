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

## 2. It is only visible under `--jdk-only-report`

The first isolated reproduction attempt PASSED 3/3, which looked like a
load-sensitive flake and would have been the wrong conclusion. The harness adds
`--jdk-only-report <dir>` to every vector under `--jdk-only` — my bare
invocation was not the same command.

```text
cratonvm --jdk-only                         -cp build RJdkEnumerations   0/3 fail
cratonvm --jdk-only --jdk-only-report DIR   -cp build RJdkEnumerations   3/3 fail
```

**A vector that passes when you run it by hand and fails in the suite is a
difference in the INVOCATION until proven otherwise.** Not the JIT either:
`--nojit` and `CRATONVM_JIT_SPILL_NARROW=0` both still fail, which ruled out
dev's narrow-safepoint-spill work in the same merge window.

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
* Not a `--jdk-only` defect in the mode sense: it reproduces because the strict
  arm turns the report on, but the report flag is available in either mode.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only --jdk-only-report /tmp/rep -cp regression-suite/build RJdkEnumerations
```

Expect `AssertionError: ConcurrentHashMap.elements(): hasMoreElements() never
terminated`. Drop `--jdk-only-report` and it passes.
