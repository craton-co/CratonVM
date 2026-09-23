# Bouncy Castle (bc-java) corpus — non-AGREE census

| | |
|---|---|
| **Measured** | 2026-09-21/22, local Windows checkout, commit `6989206e5`, CratonVM `C:/craton/CVM/target/release/cratonvm.exe` vs oracle HotSpot (openjdk 25.0.3), real JDK 25, all defaults, `run-corpus.sh` real-application-workload harness, `--mode default`, 600s cap, 18 `AllTests` JUnit-3 aggregator workloads (the adjudicable subset — see the corpus driver's own notes on why bc-java's raw `*Test` classes are not JUnit at all) |
| **Census** | AGREE=16, DIVERGE=0, **CV-BROKEN=1**, UNADJUDICATED=1, HARNESS-ERROR=0 |
| **Source** | `regression-suite/corpus/out/bc-java-default-20260922/bc-java-default-20260921-233055/results.tsv` |

**Update, same day, before this page's own commit landed:** a parallel fix (`fix(corpus): a junit arm cannot earn TIMEOUT-STALLED, so stop giving it one`, commit `0566a7b10`, not yet merged to `dev` as of this writing) establishes that a `junit`-kind workload under this corpus harness (`SbRunner`) prints exactly one line — `SBRUNNER_RESULT` — at the very end of its entire run, so **"silent at the wall" is true of every killed junit arm by construction and is not by itself evidence of a hang.** That fix's own repro is a sibling class in this same corpus (`pqc.crypto.test.AllTests`, filed below as UNADJUDICATED) which reads as "2375s silent, hung" but actually just needs a longer cap — HotSpot itself takes 888s on it, and two other PQC suites in the same sweep AGREE at 28x and 79x the HotSpot wall. The finding below was written using the OLD reasoning (silence = hang) and should be re-read in that light: it may be a real hang, or it may simply be a workload that is a few times slower than HotSpot and needs more than 600s. **Not resolved either way — re-run at `--timeout 1800` (3x the cap) before trusting either reading.**

## The one finding: `org.bouncycastle.crypto.test.AllTests` — CV-BROKEN, but see the caveat above

```
verdict:      CV-TIMEOUT-STALLED
cv_state:     TIMEOUT-STALLED  rc=124  cv_ms=600331  cv_silent_s=599
hs_state:     RAN              rc=0    hs_ms=153210
```

Killed at the 600s cap after **599 seconds with zero output** — the corpus harness's `*_silent_s` column exists precisely to separate "hung" from "merely slower than the cap," and this is unambiguously the former: essentially the entire wall was silence, not slow-but-progressing work. HotSpot runs the identical workload to completion in 153.2s. The note in `results.tsv` reads it correctly: *"That is a hang/stall, not slowness — on this VM it is very often a SIGSEGV or a livelock that printed no result line."*

**Not yet root-caused.** This is a real-application cryptography test suite (`org.bouncycastle.crypto.test`), not a synthetic fixture — worth reproducing directly (`java -cp <bc-java classpath> org.bouncycastle.crypto.test.AllTests` under the same CratonVM binary, watched for where it stops producing output or whether it actually SIGSEGVs) before guessing at a mechanism.

## Not a CratonVM finding: `org.bouncycastle.pqc.crypto.test.AllTests` — UNADJUDICATED

Both arms timed out at the 600s cap (`cv_state=TIMEOUT-STALLED`, `hs_state=TIMEOUT-BUSY`) — the oracle itself never produced a usable reference result, so there is no ground truth to compare against. `run-corpus.sh` correctly scores this `ORACLE-UNUSABLE`/`UNADJUDICATED` rather than any pass/fail verdict. Fix the workload/fixture (likely just needs a longer cap for both arms, or a smaller sub-workload) before this row can say anything about CratonVM.

**RESOLVED 2026-09-22 — it is slow, not hung, and the reason is now measured.** Run one class at a time, 29 of the suite's 36 classes complete with `fail=0 err=0`, including `SLHDSATest`, the largest; the walk was stopped after 7.5 hours inside `SnovaTest`, which a `--stack-dump-on-timeout` run shows with three threads and `main` live and unblocked inside its KAT loop. HotSpot needs **848 s** for the same 36 classes, so a 2400 s cap is 2.7x the oracle's own wall and was simply too small. The `TIMEOUT-STALLED` verdict itself was unearnable: a junit arm under `SbRunner` prints nothing between `CORPUS-START` and its closing `SBRUNNER_RESULT`, so "silent at the wall" is true of every killed junit arm by construction — `classify_arm` now answers `TIMEOUT-UNKNOWN` there instead, pinned in `run-corpus.sh selfcheck`.

The cost is localised: 91.5% of `XMSSSignatureTest`'s samples are in two hash-path methods, and the native-versus-pure-Java A/B (`ProbeSha256`, a clone of BouncyCastle's SHA-256 under a name no native matches) puts **88% of a 64-byte block in the crossing into the `processBlock` native**, not in the kernel and not in the bytecode around it — the sixteen `processWord` decodes are 384 ns of a 5 893 ns block. Full account, including the recommendation this measurement RETRACTS: `docs/jdk-only/H9-3-the-pqc-suite-is-slower-than-the-cap-not-hung-20260922.md`.

## The 16 AGREE

Every other `AllTests` aggregator agreed with HotSpot, including several with a large wall-clock gap that is explicitly NOT a metric here (this host's clock doesn't support throughput claims per-corpus-note) — e.g. `crypto.hash2curve.test.AllTests` (404s cv vs 18.6s hs) and `pqc.crypto.lms.AllTests` (497s cv vs 26.5s hs) both AGREE on outcome despite the gap.
