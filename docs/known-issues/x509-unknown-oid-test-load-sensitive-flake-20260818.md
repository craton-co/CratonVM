# One x509 unit test has failed twice under heavy parallel load, and no mechanism has been found

**Status:** OPEN (re-filed 2026-08-18). Not root-caused, **not reproduced**, and
now with its lead hypothesis disproved. Successor to
`unit-test-load-sensitive-flakes-20260815.md`, which paired this with a monitor
test that IS root-caused and fixed — see the internal record
`monitor-inflation-test-timed-its-contention-instead-of-arranging-it-FIXED-20260818`.
This page is narrower on purpose: the two had nothing in common but their
symptom, and keeping them together implied a shared cause that does not exist.

| test | crate | shape |
| --- | --- | --- |
| `x509_manager::tests::validate_chain_unknown_signature_oid_reports_not_implemented` | `cratonvm-native-builtins` | `--lib`, one process, default parallelism |

## What is established

Two failures, both on an 8-core Azure host with a **second heavy `cargo` run**
executing concurrently — once during a two-branch CI-gate comparison, once
during an unrelated build.

| arm | result |
| --- | ---: |
| alone | pass |
| whole `--lib`, `--test-threads=1` | pass |
| whole `--lib`, default parallelism, ordinary load | pass ×2 |
| `--lib x509_manager` filter | pass ×12 |
| whole `--lib`, load average ~35 | pass ×10 |
| whole `--lib`, **two other cargo suites sharing the box** | **FAIL ×2** |
| whole `--lib` at `--test-threads=64`, parallel `cargo build -j32` (2026-08-18, 32-core host) | pass ×7 |
| whole `--lib` at `--test-threads=32` with the `vm` suite running concurrently (2026-08-18) | pass ×5 |
| the test body ×640, 16 threads, one process (2026-08-18) | pass |
| the test body interleaved with `validate_chain_real_rsa_sha256_signature_passes` ×640, 16 threads (2026-08-18) | pass |

**The failure text has never been captured.** Both sightings came from CI-gate
logs that record the test NAME and the `test result:` line but not the panic
body. The test is already self-diagnosing if it ever fires again — its rejection
arm is `panic!("expected NotImplemented for Ed25519, got {:?}", other)` — so the
gap is the CI log capture, not the assertion.

## What has been ruled out

Carried forward from the original page:

* **The shared keypair `OnceLock`.** `get_or_init` is atomic and the value is
  immutable afterwards; a bad key would also fail the sibling RSA tests.
* **Certificate validity windows.** Leaf 2020→2030, anchor 2000→2049; no
  plausible scheduling delay moves `SystemTime::now()` out of range.
* **The `x509_manager` registries.** `KM_REGISTRY` / `TM_REGISTRY` / the two id
  counters are process-global, but this test builds a local
  `TrustManagerState::default()` and never registers it.

Added 2026-08-18:

* **Hypothesis 1 — a shared-name collision between the two concurrent
  "Real RSA Root" tests — is disproved, two ways.**
  *By construction:* `validate_chain(chain, trust)` is a function of the DER
  slice and a `&TrustManagerState` the caller owns. Nothing in the trust path
  caches or indexes by subject name or public key across calls; the anchors map
  lives inside that local state, and `insert_anchor` only ever writes to it.
  *Empirically:* 640 executions of the two tests deliberately interleaved across
  16 threads in one process, zero failures.
* **No process-global state anywhere on the path.** `validate_chain` reads no
  environment variable and no runtime flag, so the crate's `set_var`-using tests
  (`jboss_logmanager`, `proxy_selector`, the `JBOSS_HOME` pair) cannot reach it.
  `crypto_impl`'s statics are the JNI-facing key/cert stores, which this path
  does not touch, and `Rsa::generate_keypair` uses a local `SecureRandom::new()`
  rather than a shared RNG.

**So there is currently no known mechanism by which concurrency can change this
test's result.** That is a real finding, not a shrug: it means the next
investigation should not start by re-reading `x509_manager` for a racy global.

## What has NOT been done, and what to try next

The failure text, still. Everything above narrows *where* it cannot be; none of
it explains two observed failures.

Given that the test's own inputs are local and immutable, the two readings left
are:

1. **Something outside this test disturbed the process** — the crate's `--lib`
   carries ~25 unrelated failures on dev as of 2026-08-18, and a CI-gate log
   that records only names and the `test result:` line cannot distinguish "this
   test asserted and lost" from "this test was collateral". Confirm what the
   panic body actually was before assuming the former. **This is the cheapest
   next step and it needs no reproduction:** find the two original CI-gate runs
   and check whether their logs preserved any panic bodies at all.
2. **A defect in code the test merely passes through**, surfacing only under
   real memory/CPU pressure — the original page's hypothesis 2 (debug-build
   timing) is an amplifier, not a mechanism, so this would need naming a
   specific piece of shared state. None has been found.

Note for whoever runs the repro: on a 32-core host it did not reproduce under
any arm tried, including deliberate oversubscription. The two sightings were on
**8 cores**; a smaller box may matter more than a busier one.

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

**Detect the right thing.** `test result: FAILED` is useless as a trigger here —
this crate's `--lib` is broadly red on dev for unrelated reasons, so every run
trips it. Grep the failure list for the test name instead:

```bash
sed -n '/^failures:$/,/^test result/p' /tmp/x509-$i.log \
  | grep -q validate_chain_unknown_signature_oid && echo "caught"
```
