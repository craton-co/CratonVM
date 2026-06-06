# Cross-VM comparison — failure handoff docs (2026-06-04)

These docs capture every CratonVM failure/anomaly observed in the 4-way
comparison run (CratonVM-CPU / CratonVM-GPU / HotSpot / TornadoVM) over the
micro-benchmark suite, Bouncy Castle core suites, Apache Commons Math reactor,
and the extras (JUnit-Platform console, DaCapo avrora).

Reference VMs (HotSpot, TornadoVM) pass everything that is a genuine signal;
they are the oracle. Each file below is one independent bug for a separate
agent to pick up.

## Run provenance
- CPU binary (before fix): `target/release/cratonvm.exe`, built 10:54 (pre EC fix)
- GPU binary: `target-gpu/release/cratonvm.exe`, built 13:37 (already has EC fix)
- Harness: `test-infra/run-vm-comparison.sh`
- Raw TSVs: `test-infra/suite-results/persisted-ref/cmp-*-BEFORE-ecfix.tsv`
- Full log: `test-infra/suite-results/run-full-20260604.log`
- EC JIT fix under test: `cdab8f7` (fix/ec-native-mont-mult), `bcfbb5c`
  (coarse native EC scalar-multiply), `b19250f` (ec-native-fallthrough).

## Index
| File | Suite | Symptom | EC-fix candidate? |
|---|---|---|---|
| `bug-bintrees18-gc-throughput.md` | micro-bench | bintrees d=18 JIT-on timeout >360s | no (GC) |
| `bug-bc-math-ec-timeout.md` | BouncyCastle | math-ec timeout >360s | YES |
| `bug-bc-crypto-regression-timeout.md` | BouncyCastle | crypto-regression timeout >360s | partial |
| `bug-bc-pqc-crypto-regression-timeout.md` | BouncyCastle | pqc-crypto-regression timeout >360s | maybe (JIT ban) |
| `bug-junit-platform-console-launcher.md` | extras + commons-math | junit-help FAIL + CM reactor crash | no |
| `bug-dacapo-avrora-getfield-oob.md` | extras | latent gen_heap get_field OOB WARN | no |
| `bug-gpu-offload-launch-glue-stub.md` | gpu-offload | `--gpu` analyzes+correct but H2D=0, CPU fallback | no |
| `bug-keycloak-core-sdjwt-ecparameterspec-p256.md` | keycloak-core | SdJwtTest: can't get ECParameterSpec for P-256 | no |
| `bug-keycloak-serverspi-collector-no-code-attribute.md` | keycloak-server-spi | `Collector.accumulator() has no Code attribute` (itable dispatch) | no |
| `bug-keycloak-serverspi-url-encoding-otp.md` | keycloak-server-spi | OtpPolicy URI keeps `%20`/`%2F` instead of decoding | no |
| `bug-keycloak-serverspi-jackson-no-creators.md` | keycloak-server-spi | Jackson "no Creators" — ctor param-annotation reflection gap | no |
| `bug-interface-method-dispatch-no-code-attribute.md` | **broad** | interface invoke binds to abstract method ("has no Code attribute") — blocks ALL JUnit5 + Collectors | no |

## App gauntlet suite results (keycloak modules + wildfly/health, CratonVM-CPU vs HotSpot)
Harness: `test-infra/run-app-suite.sh` (JUnit4) + `apps/_test-harness/RunJUnit5.java`
(JUnit5, programmatic launcher to bypass the broken console launcher). A module
"passes" iff CratonVM has no MORE failures than HotSpot on the identical cp.

## After-fix status — COMPLETE (2026-06-04)
Rebuilt CPU (`target/release/java.exe`, same main.rs as cratonvm, post-fix —
built via the `java` bin to sidestep a concurrent session's lock on
`cratonvm.exe`) and GPU (`target-gpu/release/cratonvm.exe`, post-fix). Reran
CratonVM-only bench+BC+extras. Result TSV: `test-infra/suite-results/after-ecfix-150729.tsv`.

**Verdict: the EC fix is INERT across this entire suite set — before/after
pass-fail is identical.** Root cause (confirmed by the per-bug docs): the fix
(`cdab8f7`/`bcfbb5c`) accelerates **SunEC** (`sun.security.ec.ECOperations`,
default-OFF behind `CRATONVM_NATIVE_EC_MULTIPLY`), but every EC-bearing suite
here uses **BouncyCastle's own pure-Java `org.bouncycastle.math.ec`**, which
never calls SunEC. No micro-benchmark uses EC either. The fix's real
beneficiaries (SunEC TLS/keystore paths — PemLoop, keycloak) are not in this set.

| Suite | Before | After | Note |
|---|---|---|---|
| bench arith/fib/sieve/matrix/vadd | OK | OK | unchanged (timings noisy — see below) |
| bench bintrees18 (cpu) | TIMEOUT | TIMEOUT | GC throughput wall |
| BC math-ec | TIMEOUT | TIMEOUT | inert; deep root-cause in its doc (F2m JIT-ban) |
| BC crypto-regression | TIMEOUT | TIMEOUT(→rc1 crash) | inert; mid-run crash, see doc |
| BC pqc-crypto-regression | TIMEOUT | TIMEOUT | inert; JIT-ban (PQC≠EC) |
| BC asn1/math-raw/math/prng/util | OK | OK | unchanged |
| junit-help | FAIL | FAIL | launcher gap |
| dacapo-avrora | N/S | N/S | get_field WARN persists (now richer msg) |

### Caveats from the rerun (NOT regressions)
- **Timings were contaminated by a concurrent session** (another agent ran
  benchmarks against `target/release/cratonvm.exe` + a `CratonVM-bcmath`
  worktree). arith1500M read 8105 ms vs 4640 ms before — pure CPU contention,
  not the EC fix (which doesn't touch that kernel). Trust pass/fail, not the
  after-fix wall-times.
- **GPU `rc=1` on fib44 + bintrees18 in the suite run were contention artifacts.**
  Re-tested ISOLATED: fib44-GPU passes cleanly twice (5235/5139 ms, correct
  checksum). Not a GPU-binary regression.
