# Two unit tests fail only under heavy parallel load, in one process

**Status:** OPEN (2026-08-15). Not root-caused. Both are **load-sensitive
flakes, not defects in the code they cover** — each passes alone, passes under
`--test-threads=1`, and passes repeatedly under ordinary load. Filed because
they are the residual of a three-failure triage that fixed the other two, and
because a flake nobody has written down gets re-diagnosed from scratch every
time it is seen.

| test | crate | shape |
| --- | --- | --- |
| `x509_manager::tests::validate_chain_unknown_signature_oid_reports_not_implemented` | `cratonvm-native-builtins` | `--lib`, 3 563 tests in one process |
| `threading::monitor::tests::thin_lock_inflates_on_contention` | `cratonvm-vm` | `--lib`, 2 626 tests in one process |

## What is established

Azure host 2, 8 cores, `cargo test` debug builds.

**They are not deterministic.** The x509 test:

| arm | result |
| --- | ---: |
| alone (`--lib x509_manager::tests::validate_chain_unknown_signature_oid…`) | pass |
| whole `--lib`, `--test-threads=1` | pass (3 563 / 0 failed) |
| whole `--lib`, default parallelism, ordinary load | pass ×2 |
| `--lib x509_manager` filter, default parallelism | pass ×12 |
| whole `--lib`, default parallelism, **load average ~35** | pass ×10 |
| whole `--lib`, default parallelism, **two other cargo suites sharing the box** | **FAIL ×2** |

The two failures were both observed while a second heavy `cargo` run was
executing concurrently — once during a two-branch CI-gate comparison, once
during an unrelated build. **Twenty-four deliberate reproduction attempts
afterwards did not reproduce it** — 12 filtered runs and 12 full-`--lib` runs,
ten of the latter with the box at load average 34-37 from other work. The monitor test
behaved identically: it failed inside a full `-p cratonvm-vm` run at load ~36 and
passed 3/3 alone immediately after.

**`--test-threads=1` passing is the load-bearing measurement.** It says the
failure needs *concurrency*, not merely the other tests having run: a
process-global that one test corrupts for another would still fail serially.

## What has been ruled out

* **The shared keypair `OnceLock`.** `shared_rsa_root()` /
  `shared_ecdsa_root()` cache `Rsa::generate_keypair(1024)` in a `OnceLock`.
  `get_or_init` is atomic and the value is immutable afterwards, so it cannot
  hand two tests different keys. A bad generated key would also fail the two
  sibling RSA tests, and it does not.
* **Certificate validity windows.** `validate_chain` reads `SystemTime::now()`
  once and compares against `not_before_secs` / `not_after_secs`. The fixture's
  leaf is valid 2020-01-01 → 2030-01-01 and its anchor 2000 → 2049, so no
  plausible scheduling delay moves the clock out of range.
* **The `x509_manager` registries.** `KM_REGISTRY` / `TM_REGISTRY` /
  `NEXT_KM_ID` / `NEXT_TM_ID` are process-global `RwLock`s, but this test builds
  a local `TrustManagerState::default()` and never registers it.

## What has NOT been done

The failure text was never captured. Both observations came from CI-gate logs
that record the test NAME and the `test result:` line but not the panic body,
and neither has been reproduced since under instrumentation. **That is the next
step and it is the only one worth taking first:** loop the full `--lib` under
deliberate CPU contention (a parallel `cargo build -j8` is what both original
sightings had in common) with output captured per run, until one fails. Guessing
a mechanism from the code without that text is how the wrong global gets
"fixed".

Two hypotheses worth testing once the text exists, in this order:

1. **A shared-name collision between concurrent tests.**
   `validate_chain_unknown_signature_oid_reports_not_implemented` and
   `validate_chain_real_rsa_sha256_signature_passes` both build a root whose
   subject CN is `"Real RSA Root"` from the *same* cached SPKI, differing only
   in key-usage bits. Anything in the trust path that caches or indexes by
   subject name or by public key would let one test's anchor answer for the
   other's — and only when they overlap in time.
2. **Debug-build timing.** RSA-1024 keygen and chain validation are far slower
   unoptimised; under contention the two tests' windows overlap for much longer
   than they do in a release build, which is consistent with the failure being
   invisible except when a second cargo job is also running.

## Repro

```bash
# the arms that PASS, which is what makes this a flake report and not a bug report
cargo test -p cratonvm-native-builtins --lib \
  x509_manager::tests::validate_chain_unknown_signature_oid_reports_not_implemented
cargo test -p cratonvm-native-builtins --lib -- --test-threads=1

# the arm that has failed twice: full lib, default parallelism, box already busy
cargo build --workspace -j8 &          # the contention both sightings had
for i in $(seq 1 20); do
  cargo test -p cratonvm-native-builtins --lib > /tmp/x509-$i.log 2>&1 || {
    echo "caught in run $i"; break; }
done
```
