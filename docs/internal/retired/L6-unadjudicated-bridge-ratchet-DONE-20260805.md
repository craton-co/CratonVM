# L6 — Ratchet the unadjudicated `Bridge` registrations — **DONE 2026-08-05**

Retired from `docs/feature-designs/jdk-only-wave2/`. The gate exists, was shown
to fail on an injected unadjudicated `Bridge`, passes on the tree it was frozen
against, and the baseline is committed with its JDK version.

**Owned:** `regression-suite/`, `scripts/`, `.github/workflows/ci.yml` (two
additive steps). No production source was changed.
**Evidence record:** [`native-kind-is-ambient-and-defaults-to-syntheticstub.md`](../known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md)
— still **OPEN**, and stays open. This lane pins the number; it does not
reclassify a single native.

## What shipped

| File | What |
|---|---|
| `scripts/jdk-only-bridge-ratchet.py` | the gate: census → adjudication block → score against the baseline. Also `--emit-json` and `--selftest`. |
| `scripts/baselines/jdk-only-bridge-ratchet.json` | the committed baseline, keyed `<jdk-feature>/<os>`. |
| `scripts/baselines/README.md` | the rules: never hand-edit a number, `SLACK` stays 0, record why it moved. |
| `regression-suite/bridge-ratchet.sh` | the runner: self-test, boot the VM against a real JDK, take the census, gate it. |
| `scripts/jdk-only-adjudicate.py` | extended with §7, the same block as text and via `--json` — **imported from the gate**, not reimplemented. |
| `regression-suite/README.md` | a section on the gate: what it asserts, the exit codes, how to re-freeze. |
| `.github/workflows/ci.yml` | **both halves blocking**: the hermetic self-test in `jdk-only-blockers-selftest`, the measured gate in `build-and-test`'s ubuntu leg beside `Synthetic-stub ratchet`. The advisory `jdk-only` matrix also runs it, as the multi-JDK/OS probe. |

## The decision the brief asked for

The brief offered two hostings and asked that the choice be stated in the test's
doc comment. It is, at the top of `scripts/jdk-only-bridge-ratchet.py`:
**regression-suite gate**, because the question needs a real JDK image at
measurement time and `cargo test` has none. The committed-census-artefact
alternative was rejected — an 11,916-row snapshot keyed to one JDK build rots,
and a rotting baseline is worse than none.

## What it asserts, and the one thing the brief did not ask for

1. `bridge.without_acc_native <= baseline`, `SLACK = 0`. The brief's assertion.
2. `bridge.shadows_bytecode <= baseline`, `SLACK = 0`. **Added.** It is the
   subgroup that has already produced a defect: a `Bridge` shadowing concrete
   bytecode reaches §7 step 3's decline, which used to fall through to
   `UnsatisfiedLinkError` instead of to the bytecode — that is how `--jdk-only`
   came to be unable to start a thread
   ([`jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md`](jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md)).
   4,755 rows can reach it, and the aggregate ratchet alone would let a shadow
   trade places with an abstract-method intercept invisibly.
3. `total_rows >= 8_000`. The vacuity floor, labelled in the code, in the
   baseline README and in the failure message as a **collapse detector, not a
   measurement** — the same reasoning as `essential_registry_is_populated`.

## Two things the brief's plan did not account for

**The baseline needs an OS in its key, not just a JDK feature.** The brief says
to commit the baseline "with the JDK feature version beside it". That is
necessary and not sufficient: the registrars are platform-conditional, so a
Linux baseline scoring a Windows census is the same "silently combines two
different worlds" defect `scripts/jdk-only-census.sh`'s header exists to not
repeat. The key is `<jdk-feature>/<os>`; a census with no matching entry is
**refused (exit 2), never a pass**, and the CI step reports that refusal as a
gap rather than swallowing it. Only `25/linux` is committed, so three of the
four `jdk-only` matrix legs are currently ungated and say so.

**The workload is not `JdkOnlyCensusLoadProbe`, and that was measured, not
assumed.** The brief inherits the record's recipe (`-cp probes
JdkOnlyCensusLoadProbe`). Neither number this gate freezes depends on the
workload: every registration is made inside `SharedVm::new` before `main` runs,
and `image_declaring_method` is answered by parsing class-path bytes without
loading anything. Verified — the adjudication block from the breadth-first probe
is **byte-identical** to a one-line probe's, on JDK 25.0.3 / linux.

That matters because `JdkOnlyCensusLoadProbe` opens sockets, resolves DNS and
runs executors, and it **hung in its `net` section on 1 of 3 runs** on a loaded
build host. A hung probe writes no census, which the runner then has to report
as a missing prerequisite. A gate that flakes on a workload whose output it does
not read is a gate people learn to ignore, so the runner generates its own
one-line probe into a scratch directory and makes that directory the entire
class path. (That last part is load-bearing for a second reason:
`find_class_bytes_delegated` searches the application class path as well as the
image, so a shared `-cp probes` would make `image_has_class` depend on whatever
was lying there.)

## Measured, and frozen — dev `d010d611b4`, JDK 25.0.3, linux, `--real-jdk`

**11,916 registrations.** 687 `Intrinsic`, 10,842 `Bridge`, 387 `SyntheticStub`.

| what the image says about the `Bridge` target | rows | share |
|---|---:|---:|
| `ACC_NATIVE` — a genuine bridge (§1.5) | 773 | 7% |
| concrete bytecode — a **shadow** | 4,755 | 44% |
| abstract method — intercepts every implementor | 1,321 | 12% |
| class present, method **not declared** | 2,497 | 23% |
| class absent from the image | 1,496 | 14% |
| **no `ACC_NATIVE` target** | **10,069** | **93%** |

The brief's table said 10,084 of 10,844, measured 2026-08-04. The tree has moved
since (L1, L2, L9 and the `String` residuals landed); the shape has not.

**`kind_stated` is no longer false on every row, and it is still false on every
`Bridge` row.** 9 of the 687 `Intrinsic` rows now state their kind — the
`java/lang/String` natives L9 migrated to `register_with_kind`. All 10,842
`Bridge` rows, and all 10,069 unadjudicated ones, still inherit it. That is the
number L5's migration has to move, and it is now gated.

## Step 4 — verify by injection

Done twice, at two levels, because the brief is right that a guard never shown
to fail is decoration.

**Permanently, hermetically, on every run.** `--selftest` is 14 checks and runs
before anything else in `bridge-ratchet.sh`, so a broken gate stops the census
being taken at all rather than producing a green number. It injects a `Bridge`
onto a method with concrete bytecode into a synthetic census and requires exit 1
from **both** ratchets; injects one onto an absent class and requires exit 1
from the aggregate only (proof the two ratchets are not one assertion wearing
two names); injects an *adjudicated* `Bridge` and requires exit 0, so the gate
is shown not to be merely always-red; collapses the registry and requires the
vacuity floor to fire; and pins all five refusal paths to exit 2. It is wired
into the **blocking** CI job, not the advisory one.

**Once, against the real registrar.** `r.register("java/util/BitSet", "flip",
"(II)V", …)` inserted immediately under `register_jmx_natives`'s
`set_category(Bridge)` — so the kind is *inherited*, which is the
`kind_stated: false` shape the record calls the dangerous one — and
`BitSet.flip(II)V` carries concrete bytecode in the JDK 25 image. Rebuilt and
re-ran the gate as a two-arm A/B — same binary shape, one changed line between
the arms:

```
BRIDGE-RATCHET REGRESSION: Bridge registrations with no ACC_NATIVE target: 10070,
  exceeding the frozen baseline of 10069 (slack 0) by 1.
BRIDGE-RATCHET REGRESSION: Bridge registrations shadowing concrete bytecode: 4756,
  exceeding the frozen baseline of 4755 (slack 0) by 1.
BRIDGE-RATCHET: FAIL          (exit 1)
```

| | rows | `Bridge` | no `ACC_NATIVE` | shadows bytecode | gate |
|---|---:|---:|---:|---:|---|
| **A** injected | 11,917 | 10,843 | **10,070** | **4,756** | **FAIL (exit 1)** |
| **B** reverted | 11,916 | 10,842 | 10,069 | 4,755 | PASS (exit 0) |

Both ratchets fired, by exactly one, which is the injection's signature rather
than a coincidence — and the row is in arm A's census and absent from arm B's:

```json
{"class": "java/util/BitSet", "name": "flip", "descriptor": "(II)V",
 "kind": "bridge", "kind_stated": false, "registered_by": "native-builtins/src/jmx.rs:515",
 "image_declaring_method": {"image_has_class": true, "declared": true,
                            "acc_native": false, "has_code": true}}
```

Reverted (`git checkout -- native-builtins/src/jmx.rs`), rebuilt, gate green
again at 10,069 / 4,755. `git status` is clean on that file.

**The A/B also settled a question the CI wiring depends on.** Both arms were
**debug** builds, because the shared host OOM-killed the `lto=fat` release link
at `MemAvailable 0` (SIGKILL, no compile error — the failure mode the lane guide
warns about). Arm B's adjudication block is **byte-identical to the
release-built one the baseline was frozen from**, so the `jdk-only` CI job —
which builds debug to measure provenance rather than throughput — is scoring the
same numbers the baseline was taken from. A profile-sensitive census would have
made this gate quietly wrong in CI and right locally.

## Determinism

Three censuses on the same release binary and image: runs 1 and 2
byte-identical in the adjudication block. Run 3 is the hung `net` section
described above — a VM defect the runner now cuts off with `TIMEOUT` and reports
as exit 3, distinct from both a pass and a ratchet failure. Two further censuses
on the debug binary reproduce the same block.

## What this lane deliberately did not do

Nothing was reclassified and no native's kind changed. Contract §8 and the
`native-collections` `JDK-ONLY-CLASSIFY` marker both forbid it as a bulk edit,
and the record's blast-radius section explains why both directions of the
mistake are silent at the point of the mistake. The point of this lane is that
L5's subsystem-sized migrations now move a number that CI prints — which was
the brief's own argument for doing it early.
