# One x509 unit test has failed twice under heavy parallel load, and no mechanism has been found

**Status:** OPEN (re-filed 2026-08-18, advanced 2026-08-20). Still **not
reproduced**. Two things changed on 2026-08-20: the failure text is now known to
be **unrecoverable**, so nobody should look for it again; and the suite's one
load-sensitive process-wide hazard has been **named** — the page's previous
rule-out of it does not hold. Successor to
`unit-test-load-sensitive-flakes-20260815.md`, which paired this with a monitor
test that IS root-caused and fixed — see
`monitor-inflation-test-timed-its-contention-instead-of-arranging-it-FIXED-20260818.md`.

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
| **whole `--lib`, EIGHT-core host, workspace release rebuild in parallel, load average 8–15 (2026-08-20)** | **pass ×8** |
| **whole `--lib`, EIGHT-core host, ambient load 4.3–29.5, current dev with the `environ` UB REMOVED (2026-08-21)** | **pass ×25** |

The 2026-08-21 row is the same arm re-run after the UB was removed, and it is
a different question rather than a longer version of the same one: the page now
names that UB as the suite's one known load-sensitive hazard, so an arm with it
gone is the first arm that is not confounded by it. 25 consecutive runs, every
one `4134 passed; 0 failed`, at ambient load averages from 4.31 to 29.46 on 8
cores (other sessions' builds, not manufactured), wall 71–134 s against ~170 s
idle. It does not reproduce with the mechanism removed either — which is
consistent both with the UB never having been the cause and with two sightings
being too few to expect a hit in 25 runs. **Do not read it as an exoneration.**

The 2026-08-20 row is the arm the page asked for — the sightings were on 8 cores and
every earlier reproduction attempt was on a 32-core host, so "a smaller box may
matter more than a busier one" was the open lead. It does not: 8 runs of the
page's own recipe, on 8 cores, at load averages of 8.1–14.5 with a full
workspace rebuild churning beside it, and the test passed every time. Runs took
95–201 s each against 170 s idle, so the contention was real and variable.

## The failure text is gone — stop looking for it

The previous revision called this "the cheapest next step and it needs no
reproduction: find the two original CI-gate runs and check whether their logs
preserved any panic bodies at all." Done, 2026-08-20, and the answer is no:

```
grep -rn "validate_chain_unknown_signature_oid_reports_not_implemented \.\.\. FAILED" /data   -> 0 hits
grep -rn "validate_chain_unknown_signature_oid_reports_not_implemented stdout"        /data   -> 0 hits
```

About twenty logs across the host mention the test; every one of them is a
PASSING run. No surviving artefact anywhere records it failing, and none
contains a panic body for it. Whatever the two sightings were, their text did
not outlive them. **This avenue is closed** — the next person should spend their
time on the two below instead.

## The premise that the crate is broadly red is stale

The previous revision warned that "`test result: FAILED` is useless as a trigger
here — this crate's `--lib` is broadly red on dev for unrelated reasons, so
every run trips it", counting ~25 unrelated failures on 2026-08-18. On dev
`0902b7def` (2026-08-20) that is no longer true:

```
test result: FAILED. 4130 passed; 1 failed; 7 ignored; finished in 169.71s
```

One failure, and it is
`lang_class::tests::null_receiver_on_an_instance_field_outranks_the_access_refusal`
— unrelated to this path. Keying detection on the test NAME is still the right
thing to do, but the noise floor it was defending against is now a single known
row, which makes a fresh sighting far easier to spot.

## The test is deterministic given a sane clock

This is a structural argument rather than another passing run, and it is what
turns the previous revision's "no known mechanism" into something the next
investigation can actually use.

**The OID dispatch is total.** `verify_one_link`'s final `else` returns
`NotImplemented { at, oid }` for a *totally unrecognised* OID, and the arm above
it returns the same for the known-but-unimplemented set that contains
`OID_SIG_ED25519`. There is no OID that reaches the cryptographic verifier by
accident and no registry that could make one — the dispatch is a chain of `==`
against `const` byte slices, so nothing any other test does can add an Ed25519
implementation at runtime. It follows that the assertion can only lose if
`validate_chain` returns **before** the dispatch, or returns `Ok`.

Everything that can do that is a pure function of two inputs:

* **the DER bytes**, which are a pure function of the shared RSA keypair;
* **`SystemTime::now()`**, read once in `validate_ordered_chain` step 2.

`validate_chain` takes `&TrustManagerState` and the test owns a local
`TrustManagerState::default()`. The module's only process-global state is
`KM_REGISTRY` / `TM_REGISTRY` / `NEXT_KM_ID` / `NEXT_TM_ID` plus the two
identity maps, and this path reads none of them. The rebuild fallback
(`rebuild_path`) is structured to turn a rejection only into an acceptance and
returns `None` when the presented order already is the path, which it is here.

**The keypair is sound, quantitatively.** The previous revision ruled out the
shared `OnceLock` on the grounds that "a bad key would also fail the sibling RSA
tests" — an argument that cannot be checked against a CI log that lists only
names. The stronger form: `Rsa::gen_prime` accepts a candidate only after
`is_probably_prime(&candidate, 20, rng)`, so the chance of a composite modulus
factor is at most 4⁻²⁰ ≈ 10⁻¹². Two sightings in ~50 runs is ten orders of
magnitude away from that. The keypair is not the mechanism.

**The clock has a four-year margin**, and one hole. The leaf is
2001-01-01 → 2030-01-01 against a 2026 clock, so no scheduling delay moves it
out of range; the root is a stored anchor and is skipped by the clock check
entirely. The hole is the read itself:

```rust
let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs() as i64)
    .unwrap_or(0);
```

If `duration_since` ever fails, `now` silently becomes **0** and *every*
non-anchor certificate in *every* chain is rejected `NotYetValid`. That needs a
pre-1970 clock so it is not this flake, but it is the only way the pure path can
produce a wrong verdict, and it fails to a maximally confusing error. Worth
fixing on its own account.

## The rule-out of the `set_var` tests does not hold

The previous revision dismissed the crate's environment-mutating tests like
this: "`validate_chain` reads no environment variable and no runtime flag, so
the crate's `set_var`-using tests (`jboss_logmanager`, `proxy_selector`, the
`JBOSS_HOME` pair) cannot reach it."

That is an argument about variable **visibility**. The hazard is a **data race
on the environment block**. `std::env::set_var` and `remove_var` are documented
as sound only in single-threaded programs, and are `unsafe` in edition 2024, for
exactly this reason: glibc's `setenv` may `realloc` the `environ` array and free
the old one, so a concurrent `getenv` on any other thread — including calls made
from inside libc, the runtime, or the panic/backtrace machinery — can read freed
memory. It does not matter that `validate_chain` never asks for a variable.

This suite does that, at scale, while ~4,130 tests run on parallel threads in
the same process:

| site | `environ` mutations per call |
| --- | ---: |
| `proxy_selector::tests::with_proxy_env` | up to **16** — 8 `remove_var`, up to 8 `set_var`, then 8 restores |
| `lib.rs::tests::with_jboss_env` | 2 per call, plus 2 to restore |
| `jboss_logmanager::tests` (block-2c pair) | 1 `set_var`, 2 `remove_var` |

Each site takes a lock first — and each takes a **different** lock:
`proxy_selector::env_test_lock`, `lib.rs::env_lock`, and `jboss_logmanager`'s own
`lock()` are three independent mutexes. So they do not serialise even against
each other, let alone against the other four thousand tests, none of which hold
any of them. The comment on `with_jboss_env` already records one bug this
pattern caused (assertions that "held for the wrong reason"), which is evidence
the pattern is hard to reason about rather than evidence it is contained.

**This does not prove causation** — nothing here shows a corrupted `environ`
turning `NotImplemented` into another `TrustError`, and the far more likely
manifestation is a crash or a wrong answer in a test that *does* read the
environment. What it does mean is that the previous revision's headline —
"there is currently no known mechanism by which concurrency can change this
test's result" — is no longer accurate. There is a known, load-sensitive,
process-wide mechanism in this suite, it is undefined behaviour, and it was
ruled out for a reason that does not apply.

## What to try next

1. ~~**Remove the UB**~~ — done 2026-08-20. `VmFlags` now carries
   `undeclared_edits`, so `with_thread_overrides` reaches names the inventory
   does not declare, and all three sites were converted:
   `proxy_selector::tests::with_proxy_env` (up to 16 `environ` mutations per
   call) and `lib.rs::tests::with_jboss_env` to `with_thread_overrides`, and
   `jboss_logmanager`'s block-2c pair to the guard form (`override_thread`).
   **`grep -rn "std::env::set_var\|std::env::remove_var" native-builtins/src/`
   now returns only a doc comment.** The two local mutexes that used to guard
   the writes are gone with them — they never protected anything, since the
   race was against the other four thousand tests, not against each other.

   Production reads are unchanged: `runtime_var`/`runtime_var_os` consult the
   new map only while `overrides_active()` is true, and a snapshot built from
   the real environment carries none. Guarded by
   `an_undeclared_name_is_overridable_without_writing_to_environ`, which
   asserts both directions (set, and "as if unset") and that `environ` is never
   written.

   `cargo test -p cratonvm-types` 575 passed / 0 failed;
   `cargo test -p cratonvm-native-builtins --lib` 4130 passed / 1 failed — the
   same unrelated `lang_class` row as before the change; `proxy_selector` 19/0
   and `jboss` 112/0 in isolation.

   **What this does NOT settle:** the flake still has not reproduced, so this
   removes a confound rather than proving a cause. The next sighting is now
   worth much more, because the suite no longer contains a known data race that
   could explain an arbitrary result anywhere in it.
2. ~~**Fix the `unwrap_or(0)` clock read**~~ — done 2026-08-20. The read now
   negates the error's duration instead of clamping, so a pre-epoch clock
   reports a truthful negative timestamp rather than silently becoming
   1970-01-01. The suite is unchanged either side of it: 4130 passed / 1 failed
   (the unrelated `lang_class` row) before and after.
3. Do **not** re-read `x509_manager` for a racy global, and do **not** go
   looking for the original CI-gate logs. Both are closed above.

## Repro

```bash
# the arms that PASS, which is what makes this a flake report and not a bug report
cargo test -p cratonvm-native-builtins --lib \
  x509_manager::tests::validate_chain_unknown_signature_oid_reports_not_implemented
cargo test -p cratonvm-native-builtins --lib -- --test-threads=1

# the arm that has failed twice: full lib, default parallelism, box already busy.
# Use an EIGHT-core host; 8 runs of this on 2026-08-20 did not reproduce.
cargo build --workspace --release -j8 &   # a second heavy cargo run, other profile
for i in $(seq 1 20); do
  cargo test -p cratonvm-native-builtins --lib > /tmp/x509-$i.log 2>&1
  sed -n '/^failures:$/,/^test result/p' /tmp/x509-$i.log \
    | grep -q validate_chain_unknown_signature_oid && { echo "caught in run $i"; break; }
done
```

**Detect the right thing.** As of 2026-08-21 the crate's `--lib` is **fully
green** on dev — `4134 passed; 0 failed`, on 25 consecutive runs — so the
`lang_class` row this section used to warn about is fixed, and
`test result: FAILED` is a usable trigger again. Key on the test NAME inside the
failures block anyway: it costs nothing, it names the right thing, and it keeps
working the next time the crate goes red for an unrelated reason. And if it ever
does fire, **keep the whole log**: the test's rejection arm is
`panic!("expected NotImplemented for Ed25519, got {:?}", other)`, so the body
names the actual `TrustError` — which, by the argument above, is the single
piece of evidence that would distinguish "this test asserted and lost" from
"this test was collateral".
