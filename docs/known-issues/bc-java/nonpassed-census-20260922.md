# Bouncy Castle (bc-java) corpus — non-AGREE census

| | |
|---|---|
| **Measured** | 2026-09-21/22, local Windows checkout, commit `6989206e5`, CratonVM `C:/craton/CVM/target/release/cratonvm.exe` vs oracle HotSpot (openjdk 25.0.3), real JDK 25, all defaults, `run-corpus.sh` real-application-workload harness, `--mode default`, 600s cap, 18 `AllTests` JUnit-3 aggregator workloads (the adjudicable subset — see the corpus driver's own notes on why bc-java's raw `*Test` classes are not JUnit at all) |
| **Census** | AGREE=16, DIVERGE=0, **CV-BROKEN=1**, UNADJUDICATED=1, HARNESS-ERROR=0 |
| **Source** | `regression-suite/corpus/out/bc-java-default-20260922/bc-java-default-20260921-233055/results.tsv` |

Only one row here is an actual CratonVM-attributable finding; it is a hang/crash-shaped defect, not a plain FAIL, and is also listed on the cross-suite crashes page.

## The one finding: `org.bouncycastle.crypto.test.AllTests` — CV-BROKEN

```
verdict:      CV-TIMEOUT-STALLED
cv_state:     TIMEOUT-STALLED  rc=124  cv_ms=600331  cv_silent_s=599
hs_state:     RAN              rc=0    hs_ms=153210
```

Killed at the 600s cap after **599 seconds with zero output** — the corpus harness's `*_silent_s` column exists precisely to separate "hung" from "merely slower than the cap," and this is unambiguously the former: essentially the entire wall was silence, not slow-but-progressing work. HotSpot runs the identical workload to completion in 153.2s. The note in `results.tsv` reads it correctly: *"That is a hang/stall, not slowness — on this VM it is very often a SIGSEGV or a livelock that printed no result line."*

**Not yet root-caused.** This is a real-application cryptography test suite (`org.bouncycastle.crypto.test`), not a synthetic fixture — worth reproducing directly (`java -cp <bc-java classpath> org.bouncycastle.crypto.test.AllTests` under the same CratonVM binary, watched for where it stops producing output or whether it actually SIGSEGVs) before guessing at a mechanism.

## Not a CratonVM finding: `org.bouncycastle.pqc.crypto.test.AllTests` — UNADJUDICATED

Both arms timed out at the 600s cap (`cv_state=TIMEOUT-STALLED`, `hs_state=TIMEOUT-BUSY`) — the oracle itself never produced a usable reference result, so there is no ground truth to compare against. `run-corpus.sh` correctly scores this `ORACLE-UNUSABLE`/`UNADJUDICATED` rather than any pass/fail verdict. Fix the workload/fixture (likely just needs a longer cap for both arms, or a smaller sub-workload) before this row can say anything about CratonVM.

## The 16 AGREE

Every other `AllTests` aggregator agreed with HotSpot, including several with a large wall-clock gap that is explicitly NOT a metric here (this host's clock doesn't support throughput claims per-corpus-note) — e.g. `crypto.hash2curve.test.AllTests` (404s cv vs 18.6s hs) and `pqc.crypto.lms.AllTests` (497s cv vs 26.5s hs) both AGREE on outcome despite the gap.
