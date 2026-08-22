# `ExcTableDirectCallOracle` still fails ~8% of runs on `dev`

**Status: OPEN — 2026-08-22.** Found while gating an unrelated fix in the same
area (`emit_callee_deopt_check`, `706b104e2`). **It is not that fix's doing** —
measured identical on both sides — and it is not a new flake introduced by this
page's author. It is the probe that shipped with
`870ab73ab perf(jit): lift the static exception-table direct-call ban — default-ON`,
failing against its own change.

## What happens

```
[cratonvm] main-vm run() returned Err: Exception in thread "main"
  java/lang/ArithmeticException: / by zero
        at ExcTableDirectCallOracle.main(ExcTableDirectCallOracle.java:156)
```

Line 156 is `sink += driver(i, counter);` — the warm-up loop. So an
`ArithmeticException` escaped `driver`, and `driver` calls
`selfCatchDivZero(100, i & 1)` **outside** any `try` of its own: that callee is
supposed to catch its own `/ by zero`. A compiled callee's own handler did not
run.

The probe prints nothing before dying, so the failure is inside the warm-up
loop rather than in the verification section that follows it. Every successful
run prints identical output to HotSpot.

## Rates

Azure `vm1`, JDK 25, ZGC, `-Xmx2g`, host load 10–17 (recorded because it is a
shared box; the rate did not move across that range).

| binary | rate |
|---|---|
| `dev` @ `20fcda31e` — clean, no local changes | **2 / 25** |
| `dev` @ `20fcda31e` + `706b104e2` (this session's megamorphic-stub fix) | **2 / 25** |
| same, `--nojit` | **0 / 8** |
| a pre-`870ab73ab` build with the megamorphic-stub defect still in | 4 / 10 |

The first two rows are the controls that matter: **the rate is unchanged by the
megamorphic-stub fix**, so this is a separate defect that fix neither caused
nor closes. `--nojit` at 0/8 pins it as a JIT defect rather than a probe bug or
a host artefact.

(The last row is a *cross-binary* comparison — a different commit as well as a
different fix — so it is evidence only that the two defects are distinct in
rate, not a clean A/B. See the caution in
`fixed-suite-bugs/springboot/springboot-3gc-fails-and-hangs-20260821-RETIRED.md`
about cross-binary arms.)

## Why it is plausibly the same family

A `/ by zero` is an **implicit, signal-generated** exception. `CRATONVM_JIT_LOCAL_HANDLERS`
explicitly does not cover those — its own doc records that "a pending NPE /
AIOOBE / arithmetic SIGNAL … is a request to build a throwable rather than one"
and takes the old route. That old route is
`route_implicit_exc_through_callee` → `try_run_callee_handler`, which rebuilds
the callee's frame from the caller's outgoing argument slots and resumes its
handler — the same machinery whose *megamorphic* door was handing over the
wrong argument pointer. `870ab73ab` newly routes statically-bound calls in
methods that declare an exception table through the direct-call door, which is
what made this reachable.

That is a hypothesis, not a diagnosis. What is measured is the rate and the
controls above.

## Next step

Reproduce with the intermittency pinned rather than sampled: the tier window is
the obvious suspect, so `CRATONVM_C2_SUPERSEDE=0` and `CRATONVM_JIT_FORCE_C2=1`
are the two cheap arms to run first, then `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`
and `CRATONVM_JIT_IR_DIRECT_CALL=0` to say which door. A flake this shape has
been made deterministic before by pinning the tier.

```bash
javac -d /tmp/c probes/ExcTableDirectCallOracle.java
for i in $(seq 1 25); do
  cratonvm --java-home /data/toolchain/jdk-25 --Xmx 2g --XX:UseGc ZGC \
    -cp /tmp/c ExcTableDirectCallOracle > /tmp/c/$i.log 2>&1 || echo "fail $i"
done
```
