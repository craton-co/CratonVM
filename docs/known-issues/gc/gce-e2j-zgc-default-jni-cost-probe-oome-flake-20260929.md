# ZGC: `Gcd1JniCostProbe` at `-Xmx64m` with default flags sometimes dies with `OutOfMemoryError` in the new-string phase

> **STATUS (2026-09-29, gce ve2): OPEN -- ZGC failures: base 5/8, e1 0/8, e1c 3/8, e2 0/8 in the interleaved A/B, and 3/10 in ve2; G1 fails the same way (base 2/8, e2 1/8). Load-dependent and pre-existing.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/j): OPEN -- filed from the orchestrator's e1c
> measurement. The failure predates this round. It is a ZGC backend problem and
> is out of scope for the JNI lane. G1 showed it once on e1b, then not again.**

*Filed 2026-09-29 by gce e2/j, from the orchestrator's Linux release runs.
Severity: MEDIUM (a spurious `OutOfMemoryError` on a program whose live set is
tiny; ZGC only so far).*

## Evidence

- **Row:** `Gcd1JniCostProbe` (`tools/bench/Gcd1JniCostProbe.java`,
  `tools/probes/jni/Gcd1JniCostProbe.c`) with default flags (no JNI
  variable), `-XX:+UseZGC -Xmx64m`.
- **Failure rate:**

  | Build | Failures |
  |---|---|
  | base `adb9178bc` | 1/8 |
  | gce e1 | 0/8, then 0/16 |
  | gce e1c | 3/8, then 1/16 (interleaved) |

  On e1b the orchestrator also saw it on G1, 1/5; it was not seen on G1 again.
- **What a failing run prints:** `noop: ok`, `array-length: ok`,
  `int-region: ok`, then `OutOfMemoryError: Java heap space` in the
  `new-string` phase, with no Java frames captured. HotSpot 25 prints
  `new-string: ok` and `PASS all 4`.
- **The failing phase:** it is one native call that runs 200 000 ×
  (`NewStringUTF` + `GetStringUTFLength` + `DeleteLocalRef`). Each iteration
  allocates a 4-character String and its backing array and drops the local, so
  the live set stays near zero while the allocation rate is high. It is the
  shape a concurrent collector has to pace: the allocating thread stays inside
  one native call, as a counted mutator, for the whole phase.

## Where to look (ZGC backend, not investigated here)

- ZGC's allocation-stall and OOME ladder when one thread allocates
  continuously from inside a JNI native. The JNI allocation door is
  `jni_alloc_or_oom` in `vm/src/native/jni.rs`, which goes through the heap's
  shared OOME ladder (d10/o).
- The d10/o page `gcd-d10o-native-funnel-oome-debt-is-generational-only-20260928.md`
  and the G1 page `gcd-d10v-g1-heap-filling-probes-exit-before-any-output-with-an-uncaught-oome-20260929.md`
  describe neighbouring failures on the non-Generational backends.

## How to verify

- Run the default arm, 16 runs:
  `cratonvm -XX:+UseZGC -Xmx64m -cp /tmp/gcd1jnicost Gcd1JniCostProbe /tmp/libgcd1jnicost.so`.
- Expect stdout `PASS all 4` in 16/16, as HotSpot prints under
  `-XX:+UseSerialGC`, `G1` and `ZGC`.
- A decisive stdout row: the probe's `new-string: ok` line.
